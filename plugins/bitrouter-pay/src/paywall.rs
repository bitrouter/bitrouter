//! Server-side MPP paywall.
//!
//! [`MppPaywallHook`] is a `language_model::PreRequestHook` that makes every
//! inbound inference request pay for itself: a request that arrives without a
//! valid MPP credential is denied with a `402 Payment Required` carrying a
//! fresh Tempo charge challenge; a request that echoes back a signed credential
//! is verified on-chain (via `mpp_br::server`) before the pipeline proceeds.
//!
//! This is the server-side counterpart to the client flow in
//! [`crate::payment::mpp`] — the same `mpp-br` wire format, verified instead of
//! produced.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::{AppBuilder, Plugin, PluginId};

use mpp_br::server::{Mpp, TempoChargeMethod, TempoConfig, TempoProvider, tempo};

use crate::PayError;
use crate::chain::arc::ARC_TESTNET_USDC;

/// Arc testnet JSON-RPC endpoint used to verify x402/EIP-3009 settlement.
const ARC_RPC_URL: &str = "https://rpc.testnet.arc.network";
/// ERC-20 `Transfer(address,address,uint256)` event topic0.
const ERC20_TRANSFER_TOPIC0: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
/// USDC recipient the x402 (Proceeds) settlement pays to. A receipt is accepted
/// only if it carries a USDC `Transfer` to this address.
const ARC_PAYMENT_RECIPIENT: &str = "0xec56f2790840676a82ac11cbebb463eb28c9799a";
/// Header an x402-paying client sets to prove on-chain settlement.
const ARC_PAYMENT_TX_HEADER: &str = "x-arc-payment-tx";

/// Configuration for the server-side MPP paywall.
pub struct MppPaywallConfig {
    /// Address that collects the payment (the operator's treasury).
    pub recipient: String,
    /// HMAC secret the server signs challenges with — never leaves the server.
    pub secret_key: String,
    /// JSON-RPC endpoint used to verify the on-chain transfer.
    pub rpc_url: String,
    /// ERC-20 token address charged for inference.
    pub currency: String,
    /// EVM chain id the charge is bound to.
    pub chain_id: u64,
    /// Token decimals (USDC / pathUSD use 6).
    pub decimals: u32,
    /// Price per request, in dollars (e.g. `"0.001"`).
    pub amount: String,
    /// Realm advertised in the challenge.
    pub realm: String,
}

impl MppPaywallConfig {
    /// Defaults wired for Arc testnet USDC, charging `amount` dollars per call.
    #[cfg(feature = "arc")]
    pub fn arc_testnet(
        recipient: impl Into<String>,
        secret_key: impl Into<String>,
        amount: impl Into<String>,
    ) -> Self {
        Self {
            recipient: recipient.into(),
            secret_key: secret_key.into(),
            rpc_url: crate::chain::arc::ARC_TESTNET_RPC.to_string(),
            currency: crate::chain::arc::ARC_TESTNET_USDC.to_string(),
            chain_id: crate::chain::arc::ARC_TESTNET_CHAIN_ID,
            decimals: 6,
            amount: amount.into(),
            realm: "bitrouter".to_string(),
        }
    }
}

/// A `PreRequestHook` that enforces a per-request MPP payment.
#[derive(Clone)]
pub struct MppPaywallHook {
    mpp: Arc<Mpp<TempoChargeMethod<TempoProvider>>>,
    amount: String,
}

impl MppPaywallHook {
    /// Build the paywall from configuration. Fails if the Tempo verifier
    /// cannot be constructed (bad RPC URL or missing secret key).
    pub fn new(config: MppPaywallConfig) -> std::result::Result<Self, PayError> {
        let builder = tempo(TempoConfig {
            recipient: &config.recipient,
        })
        .rpc_url(&config.rpc_url)
        .chain_id(config.chain_id)
        .currency(&config.currency)
        .secret_key(&config.secret_key)
        .decimals(config.decimals)
        .realm(&config.realm);

        let mpp = Mpp::create(builder).map_err(|e| PayError::PaymentFailed(e.to_string()))?;

        Ok(Self {
            mpp: Arc::new(mpp),
            amount: config.amount,
        })
    }

    /// Render a fresh `WWW-Authenticate` challenge value for the configured
    /// price. A failure here is a server misconfiguration, not a client fault.
    fn challenge(&self) -> Result<String> {
        self.mpp
            .charge(&self.amount)
            .and_then(|c| c.to_header())
            .map_err(|e| BitrouterError::internal(format!("building MPP challenge: {e}")))
    }
}

/// True when a header value carries the `Payment` auth scheme (case-insensitive).
fn is_payment_scheme(value: &str) -> bool {
    value.len() > 8 && value[..7].eq_ignore_ascii_case("Payment") && value.as_bytes()[7] == b' '
}

/// Verify an x402/EIP-3009 settlement on Arc testnet.
///
/// Calls `eth_getTransactionReceipt` on the Arc RPC and accepts the tx only if
/// it exists, succeeded (`status == 0x1`), and emitted a USDC
/// `Transfer(_, ARC_PAYMENT_RECIPIENT, _)` log (i.e. the payment actually
/// landed at the operator's receiving address on the USDC contract).
async fn verify_arc_payment_tx(tx_hash: &str) -> bool {
    let tx_hash = if tx_hash.starts_with("0x") || tx_hash.starts_with("0X") {
        tx_hash.to_string()
    } else {
        format!("0x{tx_hash}")
    };

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getTransactionReceipt",
        "params": [tx_hash],
    });

    let resp = match reqwest::Client::new()
        .post(ARC_RPC_URL)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Arc RPC eth_getTransactionReceipt request failed");
            return false;
        }
    };
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Arc RPC returned an undecodable receipt body");
            return false;
        }
    };

    let receipt = &body["result"];
    if receipt.is_null() {
        return false;
    }
    if !receipt["status"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("0x1"))
    {
        return false;
    }

    // Expected indexed `to` topic: address left-padded to 32 bytes.
    let recipient = ARC_PAYMENT_RECIPIENT
        .trim_start_matches("0x")
        .to_lowercase();
    let expected_to_topic = format!("0x{:0>64}", recipient);

    let Some(logs) = receipt["logs"].as_array() else {
        return false;
    };
    logs.iter().any(|log| {
        let on_usdc = log["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(ARC_TESTNET_USDC));
        let Some(topics) = log["topics"].as_array() else {
            return false;
        };
        let is_transfer = topics
            .first()
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case(ERC20_TRANSFER_TOPIC0));
        let to_recipient = topics
            .get(2)
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case(&expected_to_topic));
        on_usdc && is_transfer && to_recipient
    })
}

#[async_trait]
impl PreRequestHook for MppPaywallHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        // x402 / EIP-3009 settlement path. A client that paid on-chain (USDC
        // `transferWithAuthorization` facilitated by Proceeds) proves it with an
        // `X-Arc-Payment-Tx` header carrying the settlement tx hash. We verify
        // the receipt directly on Arc and admit the request, bypassing the
        // mpp-br/Tempo path (which cannot broadcast a standard EVM tx on Arc).
        // A missing or unverifiable header falls through to the mpp-br flow.
        if let Some(tx) = ctx
            .headers()
            .get(ARC_PAYMENT_TX_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if verify_arc_payment_tx(tx).await {
                tracing::info!(
                    tx,
                    "MPP paywall: Arc x402 payment verified on-chain, admitting"
                );
                return Ok(HookDecision::Allow);
            }
            tracing::warn!(
                tx,
                "MPP paywall: X-Arc-Payment-Tx did not verify on Arc, falling through to mpp-br"
            );
        }

        // Read the raw `Authorization` header verbatim so the value the client
        // actually sent is visible before any scheme / parse filtering. This is
        // the exact counterpart of the value `ArcMppPayClient` logs on retry.
        let raw = ctx
            .headers()
            .get(mpp_br::AUTHORIZATION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .map(str::to_string);

        let Some(raw) = raw else {
            tracing::info!(
                header = mpp_br::AUTHORIZATION_HEADER,
                "MPP paywall: no Authorization header present, returning 402 challenge"
            );
            return Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                self.challenge()?,
            )));
        };
        tracing::debug!(
            header = mpp_br::AUTHORIZATION_HEADER,
            value = %raw,
            len = raw.len(),
            "MPP paywall: received Authorization header"
        );

        if !is_payment_scheme(&raw) {
            // Header present but not the `Payment` scheme — the client must send
            // `Authorization: Payment <credential>` (the output of
            // `mpp_br::format_authorization`).
            let scheme = raw.split_whitespace().next().unwrap_or("");
            tracing::warn!(
                scheme,
                "MPP paywall: Authorization header is not the 'Payment' scheme, re-challenging"
            );
            return Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                self.challenge()?,
            )));
        }

        let credential = match mpp_br::parse_authorization(&raw) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    value = %raw,
                    "MPP paywall: malformed credential (parse_authorization failed), re-challenging"
                );
                return Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                    self.challenge()?,
                )));
            }
        };

        match self.mpp.verify_credential(&credential).await {
            Ok(_receipt) => {
                tracing::info!("MPP paywall: payment verified on-chain, admitting request");
                Ok(HookDecision::Allow)
            }
            Err(e) => {
                // The credential parsed but on-chain verification rejected it —
                // the most common cause is a settlement that has not yet been
                // observed on the configured RPC, or a recipient / amount /
                // chain-id mismatch between the challenge and the transfer.
                tracing::warn!(
                    error = %e,
                    "MPP paywall: on-chain verify_credential failed, re-challenging"
                );
                Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                    self.challenge()?,
                )))
            }
        }
    }
}

/// A [`Plugin`] that installs the [`MppPaywallHook`] on the `language_model`
/// pipeline, so every request to the inference endpoint must fund itself.
pub struct MppPaywallPlugin {
    id: PluginId,
    hook: MppPaywallHook,
}

impl MppPaywallPlugin {
    /// Build the paywall plugin from configuration.
    pub fn new(config: MppPaywallConfig) -> std::result::Result<Self, PayError> {
        Ok(Self {
            id: PluginId::new("bitrouter-pay-paywall"),
            hook: MppPaywallHook::new(config)?,
        })
    }
}

impl Plugin for MppPaywallPlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        app.language_model_builder()
            .pre_request_hook(self.hook.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MppPaywallConfig {
        MppPaywallConfig {
            recipient: "0xBB4CB05dA6ED0780cFDd0F088EaEEd420381DE38".to_string(),
            secret_key: "test-secret".to_string(),
            rpc_url: "https://rpc.testnet.arc.network".to_string(),
            currency: "0x3600000000000000000000000000000000000000".to_string(),
            chain_id: 5042002,
            decimals: 6,
            amount: "0.001".to_string(),
            realm: "bitrouter".to_string(),
        }
    }

    #[test]
    fn challenge_carries_payment_scheme() {
        let hook = MppPaywallHook::new(test_config()).expect("build paywall hook");
        let header = hook.challenge().expect("render challenge");
        assert!(
            header.starts_with("Payment "),
            "challenge must be a parseable MPP challenge, got: {header}"
        );
        assert!(header.contains("method=\"tempo\""));
        assert!(header.contains("intent=\"charge\""));
    }

    #[test]
    fn payment_scheme_detection() {
        assert!(is_payment_scheme("Payment eyJ0b2tlbiI"));
        assert!(is_payment_scheme("payment eyJ0b2tlbiI"));
        assert!(!is_payment_scheme("Bearer brvk_abc"));
        assert!(!is_payment_scheme("Payment")); // scheme present but no token
        assert!(!is_payment_scheme(""));
    }
}
