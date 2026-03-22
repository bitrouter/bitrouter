//! Type definitions for Solana MPP session payments.
//!
//! Mirrors the TypeScript SDK's `session/Types.ts` from
//! `solana-foundation/mpp-sdk`, adapted for Rust serialization.

use serde::{Deserialize, Serialize};

/// Solana session voucher — the data that gets signed by the payer.
///
/// Fields are serialized to JSON with sorted keys and a domain separator
/// for Ed25519 signing (see [`super::solana_voucher`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolanaSessionVoucher {
    pub chain_id: String,
    pub channel_id: String,
    pub channel_program: String,
    pub cumulative_amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub meter: String,
    pub payer: String,
    pub recipient: String,
    pub sequence: u64,
    pub server_nonce: String,
    pub units: String,
}

/// A voucher with its Ed25519 (or Swig-session) signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedSolanaSessionVoucher {
    pub signature: String,
    pub signature_type: SignatureType,
    pub signer: String,
    pub voucher: SolanaSessionVoucher,
}

/// Signature algorithm used for the voucher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureType {
    #[serde(rename = "ed25519")]
    Ed25519,
    #[serde(rename = "swig-session")]
    SwigSession,
}

/// Session credential payload, discriminated on the `action` field.
///
/// Each variant corresponds to a channel lifecycle action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SolanaSessionCredentialPayload {
    /// Open a new payment channel.
    #[serde(rename = "open", rename_all = "camelCase")]
    Open {
        channel_id: String,
        open_tx: String,
        voucher: SignedSolanaSessionVoucher,
        payer: String,
        deposit_amount: String,
        authorization_mode: AuthorizationMode,
        #[serde(skip_serializing_if = "Option::is_none")]
        capabilities: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<String>,
    },
    /// Top up an existing channel with additional funds.
    #[serde(rename = "topup", rename_all = "camelCase")]
    TopUp {
        channel_id: String,
        topup_tx: String,
        additional_amount: String,
    },
    /// Off-chain voucher update (no on-chain transaction).
    #[serde(rename = "update", rename_all = "camelCase")]
    Update {
        channel_id: String,
        voucher: SignedSolanaSessionVoucher,
    },
    /// Close the payment channel.
    #[serde(rename = "close", rename_all = "camelCase")]
    Close {
        channel_id: String,
        voucher: SignedSolanaSessionVoucher,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_tx: Option<String>,
    },
}

/// Authorization mode for a Solana session channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationMode {
    #[serde(rename = "swig_session")]
    SwigSession,
    #[serde(rename = "regular_budget")]
    RegularBudget,
    #[serde(rename = "regular_unbounded")]
    RegularUnbounded,
}

/// Server-side state for a Solana payment channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolanaChannelState {
    pub channel_id: String,
    pub payer: String,
    pub recipient: String,
    pub server_nonce: String,
    pub channel_program: String,
    pub chain_id: String,
    pub authorization_mode: AuthorizationMode,
    /// Wallet address or delegated session key for swig_session mode.
    pub authority_wallet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegated_session_key: Option<String>,
    pub escrowed_amount: String,
    pub last_authorized_amount: String,
    pub last_sequence: u64,
    pub settled_amount: String,
    pub status: ChannelStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<i64>,
    pub created_at: String,
}

/// Channel lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStatus {
    Open,
    Closing,
    Closed,
    Expired,
}

/// Solana-specific session method details for challenges.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolanaSessionMethodDetails {
    pub channel_program: String,
    pub network: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_payload_open_roundtrip() {
        let payload = SolanaSessionCredentialPayload::Open {
            channel_id: "ch_123".into(),
            open_tx: "base64tx".into(),
            voucher: SignedSolanaSessionVoucher {
                signature: "sig123".into(),
                signature_type: SignatureType::Ed25519,
                signer: "pubkey123".into(),
                voucher: SolanaSessionVoucher {
                    chain_id: "solana:mainnet-beta".into(),
                    channel_id: "ch_123".into(),
                    channel_program: "prog123".into(),
                    cumulative_amount: "0".into(),
                    expires_at: None,
                    meter: "session".into(),
                    payer: "payer123".into(),
                    recipient: "recip123".into(),
                    sequence: 0,
                    server_nonce: "nonce123".into(),
                    units: "0".into(),
                },
            },
            payer: "payer123".into(),
            deposit_amount: "1000000".into(),
            authorization_mode: AuthorizationMode::SwigSession,
            capabilities: None,
            expires_at: None,
        };

        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(json.contains("\"action\":\"open\""));
        let parsed: SolanaSessionCredentialPayload =
            serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            parsed,
            SolanaSessionCredentialPayload::Open { .. }
        ));
    }

    #[test]
    fn credential_payload_update_roundtrip() {
        let json = r#"{"action":"update","channelId":"ch_1","voucher":{"signature":"sig","signatureType":"swig-session","signer":"key","voucher":{"chainId":"solana:mainnet-beta","channelId":"ch_1","channelProgram":"prog","cumulativeAmount":"500","meter":"token","payer":"alice","recipient":"bob","sequence":3,"serverNonce":"n","units":"5"}}}"#;
        let parsed: SolanaSessionCredentialPayload =
            serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            parsed,
            SolanaSessionCredentialPayload::Update { .. }
        ));
    }
}
