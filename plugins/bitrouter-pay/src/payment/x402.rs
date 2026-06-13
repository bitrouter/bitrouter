//! Proceeds x402 v2 payment loop with EIP-3009 `transferWithAuthorization`.
//!
//! Flow:
//!   POST → 402 (JSON body: x402 v2 challenge) →
//!   EIP-712 sign → PAYMENT-SIGNATURE header (base64url proof) → retry → 200.
//!
//! The client signs the USDC `transferWithAuthorization` authorization
//! off-chain.  Proceeds submits the on-chain transaction; no RPC call is
//! required from the client.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::PayError;
use crate::chain::arc::{ARC_TESTNET_CAIP2, ARC_TESTNET_CHAIN_ID, ARC_TESTNET_USDC};
use crate::wallet::ArcSigner;

const PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";

/// Parameters of a USDC EIP-3009 `transferWithAuthorization` message.
pub struct TransferAuthorization {
    pub from: Address,
    pub to: Address,
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub nonce: B256,
}

/// Build the EIP-712 typed-data JSON for a USDC `transferWithAuthorization`.
///
/// `domain_name`/`domain_version` come from the 402 challenge's `extra` field
/// (the facilitator declares the EIP-712 domain it verifies against). This is
/// the exact structure the OWS CLI (`ows sign message --typed-data`) hashes
/// and signs.
pub fn build_transfer_authorization_typed_data(
    domain_name: &str,
    domain_version: &str,
    auth: &TransferAuthorization,
) -> Value {
    serde_json::json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "TransferWithAuthorization": [
                {"name": "from", "type": "address"},
                {"name": "to", "type": "address"},
                {"name": "value", "type": "uint256"},
                {"name": "validAfter", "type": "uint256"},
                {"name": "validBefore", "type": "uint256"},
                {"name": "nonce", "type": "bytes32"}
            ]
        },
        "primaryType": "TransferWithAuthorization",
        "domain": {
            "name": domain_name,
            "version": domain_version,
            "chainId": ARC_TESTNET_CHAIN_ID,
            "verifyingContract": ARC_TESTNET_USDC
        },
        "message": {
            "from": auth.from.to_string(),
            "to": auth.to.to_string(),
            "value": auth.value.to_string(),
            "validAfter": auth.valid_after.to_string(),
            "validBefore": auth.valid_before.to_string(),
            "nonce": format!("0x{}", hex::encode(auth.nonce.0))
        }
    })
}

// ── X402Client ───────────────────────────────────────────────────────────────

/// Client-side x402 v2 payer for Proceeds paywalls.
///
/// Reads the x402 v2 JSON challenge from the 402 response body, selects the
/// `eip3009` accept entry, signs `transferWithAuthorization` via EIP-712 with
/// the OWS-backed [`ArcSigner`], and retries the request with the signed proof
/// in the `PAYMENT-SIGNATURE` header.
pub struct X402Client {
    signer: Arc<ArcSigner>,
    http: reqwest::Client,
}

impl X402Client {
    pub fn new(signer: Arc<ArcSigner>) -> Self {
        Self {
            signer,
            http: reqwest::Client::new(),
        }
    }

    /// POST to `url`, pay on 402 via EIP-3009, retry, return upstream JSON body.
    pub async fn post(&self, url: &str, body: Option<Value>) -> Result<Value, PayError> {
        let first = self.send(url, body.clone(), None).await?;
        if first.status().as_u16() != 402 {
            return parse_ok(first).await;
        }

        // Parse the x402 v2 challenge from the 402 response body. Keep it as
        // raw JSON so the selected accept entry and the resource object can be
        // echoed back exactly in the payment proof.
        let raw = first
            .text()
            .await
            .map_err(|e| PayError::InvalidChallenge(format!("failed to read 402 body: {e}")))?;
        let challenge: Value = serde_json::from_str(&raw).map_err(|e| {
            PayError::InvalidChallenge(format!("invalid x402 v2 JSON: {e} | body: {raw}"))
        })?;

        let resource = challenge.get("resource").cloned().unwrap_or(Value::Null);

        let accepted = challenge
            .get("accepts")
            .and_then(Value::as_array)
            .and_then(|accepts| {
                accepts
                    .iter()
                    .find(|a| {
                        a["scheme"].as_str() == Some("exact")
                            && a["network"].as_str() == Some(ARC_TESTNET_CAIP2)
                            && a["extra"]["assetTransferMethod"].as_str() == Some("eip3009")
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                PayError::InvalidChallenge(format!(
                    "no exact/eip3009 accept entry for {ARC_TESTNET_CAIP2} in x402 challenge"
                ))
            })?;

        let proof_b64 = self.sign_eip3009(&accepted, &resource).await?;

        let paid = self.send(url, body, Some(proof_b64)).await?;
        if !paid.status().is_success() {
            let s = paid.status();
            let t = paid.text().await.unwrap_or_default();
            return Err(PayError::PaymentFailed(format!(
                "payment retry returned {s}: {t}"
            )));
        }
        parse_ok(paid).await
    }

    /// Build the base64url-encoded x402 payment proof for an eip3009 accept
    /// entry. `accepted` and `resource` are echoed back exactly as received in
    /// the 402 challenge.
    async fn sign_eip3009(&self, accepted: &Value, resource: &Value) -> Result<String, PayError> {
        let from = self.signer.address();
        let to: Address = accepted["payTo"]
            .as_str()
            .ok_or_else(|| PayError::InvalidChallenge("missing payTo".into()))?
            .parse()
            .map_err(|e| PayError::InvalidChallenge(format!("invalid payTo address: {e}")))?;
        let amount = accepted["amount"]
            .as_str()
            .ok_or_else(|| PayError::InvalidChallenge("missing amount".into()))?
            .parse::<u128>()
            .map_err(|e| PayError::InvalidChallenge(format!("invalid amount: {e}")))?;
        let max_timeout = accepted["maxTimeoutSeconds"].as_u64().unwrap_or(300);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| PayError::SignerError(e.to_string()))?
            .as_secs();

        let valid_after = 0u64;
        let valid_before = now + max_timeout;
        let nonce = B256::from(rand::random::<[u8; 32]>());

        // Use the EIP-712 domain the facilitator declared in the challenge.
        let domain_name = accepted["extra"]["name"].as_str().unwrap_or("USD Coin");
        let domain_version = accepted["extra"]["version"].as_str().unwrap_or("2");

        let auth = TransferAuthorization {
            from,
            to,
            value: amount,
            valid_after,
            valid_before,
            nonce,
        };

        let typed_data =
            build_transfer_authorization_typed_data(domain_name, domain_version, &auth);

        let sig = self.signer.sign_typed_data(&typed_data.to_string()).await?;

        // USDC's EIP-3009 implementation uses ecrecover with v = 27 or 28.
        let mut sig_bytes = Vec::with_capacity(65);
        sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
        sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
        sig_bytes.push(if sig.v() { 28 } else { 27 });
        let sig_hex = format!("0x{}", hex::encode(&sig_bytes));

        // x402 v2 "exact"/eip3009 proof envelope: echo the challenge's
        // `resource` and selected accept entry, plus the signed authorization.
        // All numerics in the authorization are strings.
        let proof = serde_json::json!({
            "x402Version": 2,
            "resource": resource,
            "accepted": accepted,
            "payload": {
                "signature": sig_hex,
                "authorization": {
                    // lowercase to match the challenge's payTo exactly
                    "from": from.to_string().to_lowercase(),
                    "to": to.to_string().to_lowercase(),
                    "value": amount.to_string(),
                    "validAfter": valid_after.to_string(),
                    "validBefore": valid_before.to_string(),
                    "nonce": format!("0x{}", hex::encode(nonce.0)),
                }
            }
        });
        // The PAYMENT-SIGNATURE header must be base64url-encoded without padding.
        Ok(URL_SAFE_NO_PAD.encode(proof.to_string()))
    }

    async fn send(
        &self,
        url: &str,
        body: Option<Value>,
        payment_proof: Option<String>,
    ) -> Result<reqwest::Response, PayError> {
        let mut req = self.http.post(url);
        if let Some(j) = body {
            req = req.json(&j);
        }
        if let Some(proof) = payment_proof {
            req = req.header(PAYMENT_SIGNATURE, proof);
        }
        req.send()
            .await
            .map_err(|e| PayError::UpstreamError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256, keccak256};

    use crate::chain::arc::{ARC_TESTNET_CHAIN_ID, ARC_TESTNET_USDC};

    fn domain_separator(name: &str) -> B256 {
        let typehash = keccak256(
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let usdc: Address = ARC_TESTNET_USDC.parse().ok().unwrap_or(Address::ZERO);
        let mut buf = Vec::new();
        buf.extend_from_slice(typehash.as_ref());
        buf.extend_from_slice(keccak256(name.as_bytes()).as_ref());
        buf.extend_from_slice(keccak256(b"2").as_ref());
        buf.extend_from_slice(&U256::from(ARC_TESTNET_CHAIN_ID).to_be_bytes::<32>());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(usdc.as_ref());
        keccak256(&buf)
    }

    /// On-chain `DOMAIN_SEPARATOR()` of Arc testnet USDC, fetched via eth_call.
    const ONCHAIN: &str = "361191522483d32a83e70ae7183b4b9629442c13a78bc9921d6f707911c8c6b0";

    #[test]
    fn usdc_domain_name_matches_onchain_separator() {
        let usdc = hex::encode(domain_separator("USDC").0);
        let usd_coin = hex::encode(domain_separator("USD Coin").0);
        println!("USDC     => 0x{usdc}");
        println!("USD Coin => 0x{usd_coin}");
        println!("on-chain => 0x{ONCHAIN}");
        assert!(
            usdc == ONCHAIN || usd_coin == ONCHAIN,
            "neither domain name matches the on-chain separator"
        );
    }

    /// Verify that a signature produced by the OWS CLI over our typed data
    /// recovers to the agent wallet address (captured from a live run).
    #[test]
    fn ows_cli_signature_recovers_to_wallet() {
        let from: Address = "0xBB4CB05dA6ED0780cFDd0F088EaEEd420381DE38"
            .parse()
            .ok()
            .unwrap_or(Address::ZERO);
        let to: Address = "0xEC56f2790840676A82ac11CBebb463EB28C9799A"
            .parse()
            .ok()
            .unwrap_or(Address::ZERO);
        let nonce: B256 = "0xe505473e1c1363d7524c6ba8633c3cf3f85421bd9600f5caaea828a3f7e6a1e0"
            .parse()
            .ok()
            .unwrap_or(B256::ZERO);

        // struct hash
        let typehash = keccak256(
            b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(typehash.as_ref());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(from.as_ref());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(to.as_ref());
        buf.extend_from_slice(&U256::from(1000u64).to_be_bytes::<32>());
        buf.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        buf.extend_from_slice(&U256::from(1781314482u64).to_be_bytes::<32>());
        buf.extend_from_slice(nonce.as_ref());
        let struct_hash = keccak256(&buf);

        let mut digest_input = Vec::new();
        digest_input.push(0x19);
        digest_input.push(0x01);
        digest_input.extend_from_slice(domain_separator("USDC").as_ref());
        digest_input.extend_from_slice(struct_hash.as_ref());
        let digest = keccak256(&digest_input);

        // Signature captured from the live OWS CLI run over the same typed data.
        let sig_hex = "bb0980f8f1a2b6544ed06ba2476b2c88d1374d80d2c0f9f634efc4796bfb40d461ab11a9727a0fb34e0efd60307168fa83b5b366ce9f98b6f544773b2720a6de1b";
        let sig_bytes = hex::decode(sig_hex).ok().unwrap_or_default();
        let r = U256::from_be_slice(&sig_bytes[..32]);
        let s = U256::from_be_slice(&sig_bytes[32..64]);
        let y_parity = sig_bytes[64] == 28;
        let sig = alloy::primitives::Signature::new(r, s, y_parity);

        let recovered = sig
            .recover_address_from_prehash(&digest)
            .ok()
            .unwrap_or(Address::ZERO);
        println!("expected  => {from}");
        println!("recovered => {recovered}");
        assert_eq!(recovered, from, "CLI signature does not recover to wallet");
    }
}

async fn parse_ok(resp: reqwest::Response) -> Result<Value, PayError> {
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(PayError::UpstreamError(format!("upstream {s}: {t}")));
    }
    resp.json()
        .await
        .map_err(|e| PayError::UpstreamError(e.to_string()))
}
