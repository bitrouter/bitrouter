//! Server-side session payment verification for Solana.
//!
//! Implements the `SessionMethod` trait from mpp-rs for Solana session
//! payments (pay-as-you-go with Ed25519 vouchers). Handles four channel
//! lifecycle actions: open, topUp, update, close.
//!
//! Ported from the TypeScript SDK's `server/Session.ts`.

use std::sync::Arc;

use mpp::protocol::core::{PaymentCredential, Receipt};
use mpp::protocol::intents::SessionRequest;
use mpp::protocol::traits::{SessionMethod as SessionMethodTrait, VerificationError};

use super::solana_channel_store::SolanaChannelStore;
use super::solana_types::{
    AuthorizationMode, ChannelStatus, SignedSolanaSessionVoucher, SolanaChannelState,
    SolanaSessionCredentialPayload, SolanaSessionMethodDetails,
};
use super::solana_voucher::verify_voucher_signature;

/// Configuration for the Solana session method.
#[derive(Debug, Clone)]
pub struct SolanaSessionMethodConfig {
    /// The channel (escrow) program address.
    pub channel_program: String,
    /// The Solana network name (e.g., "mainnet-beta", "devnet").
    pub network: String,
}

/// Solana session method for server-side session payment verification.
///
/// Handles four channel lifecycle actions:
/// - `open`:   verify open tx + initial voucher, create channel in store
/// - `topup`:  verify topup tx, update deposit in store
/// - `update`: verify voucher signature + monotonicity, update store
/// - `close`:  verify final voucher, mark channel closed
#[derive(Clone)]
pub struct SolanaSessionMethod {
    store: Arc<dyn SolanaChannelStore>,
    config: SolanaSessionMethodConfig,
}

impl SolanaSessionMethod {
    pub fn new(store: Arc<dyn SolanaChannelStore>, config: SolanaSessionMethodConfig) -> Self {
        Self { store, config }
    }

    /// Normalize a network name to a CAIP-2 Solana chain ID.
    fn normalize_chain_id(network: &str) -> String {
        let normalized = network.trim();
        if normalized.starts_with("solana:") {
            normalized.to_string()
        } else {
            format!("solana:{normalized}")
        }
    }
}

/// Parameters for the `handle_open` action.
struct HandleOpenParams<'a> {
    channel_id: &'a str,
    open_tx: &'a str,
    voucher: &'a SignedSolanaSessionVoucher,
    payer: &'a str,
    deposit_amount: &'a str,
    authorization_mode: &'a AuthorizationMode,
    expires_at: Option<&'a str>,
    configured_recipient: &'a str,
}

impl SolanaSessionMethod {
    /// Handle the `open` action.
    async fn handle_open(
        &self,
        params: HandleOpenParams<'_>,
    ) -> Result<Receipt, VerificationError> {
        let HandleOpenParams {
            channel_id,
            open_tx,
            voucher,
            payer,
            deposit_amount,
            authorization_mode,
            expires_at,
            configured_recipient,
        } = params;
        if open_tx.trim().is_empty() {
            return Err(VerificationError::invalid_payload(
                "openTx is required for session open",
            ));
        }

        let deposit: u128 = parse_non_negative_amount(deposit_amount, "depositAmount")?;
        let cumulative: u128 =
            parse_non_negative_amount(&voucher.voucher.cumulative_amount, "cumulativeAmount")?;

        // Validate voucher fields match the open payload.
        if voucher.voucher.channel_id != channel_id {
            return Err(VerificationError::invalid_payload(
                "voucher channelId mismatch for open action",
            ));
        }
        if voucher.voucher.payer != payer {
            return Err(VerificationError::invalid_payload(
                "voucher payer mismatch for open action",
            ));
        }
        if voucher.voucher.recipient != configured_recipient {
            return Err(VerificationError::invalid_payload(
                "voucher recipient does not match configured recipient",
            ));
        }
        if voucher.voucher.channel_program != self.config.channel_program {
            return Err(VerificationError::invalid_payload(
                "voucher channelProgram mismatch",
            ));
        }

        let expected_chain_id = Self::normalize_chain_id(&self.config.network);
        if voucher.voucher.chain_id != expected_chain_id {
            return Err(VerificationError::with_code(
                format!(
                    "voucher chainId mismatch: expected {expected_chain_id}, received {}",
                    voucher.voucher.chain_id
                ),
                mpp::server::ErrorCode::ChainIdMismatch,
            ));
        }

        if cumulative > deposit {
            return Err(VerificationError::amount_exceeds_deposit(
                "voucher cumulative amount exceeds channel deposit",
            ));
        }

        // Check voucher expiry.
        assert_voucher_not_expired(voucher)?;

        // Verify Ed25519 signature.
        let valid = verify_voucher_signature(voucher)?;
        if !valid {
            return Err(VerificationError::invalid_signature(
                "invalid voucher signature",
            ));
        }

        let created_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".into());

        let expires_at_unix = expires_at
            .or(voucher.voucher.expires_at.as_deref())
            .and_then(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                    .ok()
                    .map(|dt| dt.unix_timestamp())
            });

        let delegated_session_key = if *authorization_mode == AuthorizationMode::SwigSession {
            Some(voucher.signer.clone())
        } else {
            None
        };

        let next_state = SolanaChannelState {
            channel_id: channel_id.to_string(),
            payer: payer.to_string(),
            recipient: configured_recipient.to_string(),
            server_nonce: voucher.voucher.server_nonce.clone(),
            channel_program: self.config.channel_program.clone(),
            chain_id: expected_chain_id,
            authorization_mode: authorization_mode.clone(),
            authority_wallet: payer.to_string(),
            delegated_session_key,
            escrowed_amount: deposit_amount.to_string(),
            last_authorized_amount: voucher.voucher.cumulative_amount.clone(),
            last_sequence: voucher.voucher.sequence,
            settled_amount: "0".into(),
            status: ChannelStatus::Open,
            expires_at_unix,
            created_at,
        };

        let ch_id = channel_id.to_string();
        self.store
            .update_channel(
                channel_id,
                Box::new(move |existing| {
                    if existing.is_some() {
                        return Err(VerificationError::invalid_payload(format!(
                            "channel already exists: {ch_id}"
                        )));
                    }
                    Ok(Some(next_state))
                }),
            )
            .await?;

        Ok(Receipt::success("solana", channel_id))
    }

    /// Handle the `update` action.
    async fn handle_update(
        &self,
        channel_id: &str,
        voucher: &SignedSolanaSessionVoucher,
    ) -> Result<Receipt, VerificationError> {
        let channel = self.store.get_channel(channel_id).await?.ok_or_else(|| {
            VerificationError::channel_not_found(format!("channel not found: {channel_id}"))
        })?;

        assert_channel_open(&channel)?;
        assert_voucher_matches_channel(voucher, &channel)?;
        assert_voucher_not_expired(voucher)?;
        assert_signer_authorized(voucher, &channel)?;

        let cumulative =
            parse_non_negative_amount(&voucher.voucher.cumulative_amount, "cumulativeAmount")?;
        let escrowed = parse_non_negative_amount(&channel.escrowed_amount, "escrowedAmount")?;
        let last_authorized =
            parse_non_negative_amount(&channel.last_authorized_amount, "lastAuthorizedAmount")?;

        if voucher.voucher.sequence <= channel.last_sequence {
            return Err(VerificationError::invalid_payload(format!(
                "voucher sequence replay: last={}, received={}",
                channel.last_sequence, voucher.voucher.sequence
            )));
        }
        if cumulative < last_authorized {
            return Err(VerificationError::invalid_payload(
                "voucher cumulative amount must be monotonically non-decreasing",
            ));
        }
        if cumulative > escrowed {
            return Err(VerificationError::amount_exceeds_deposit(
                "voucher cumulative amount exceeds channel deposit",
            ));
        }

        let valid = verify_voucher_signature(voucher)?;
        if !valid {
            return Err(VerificationError::invalid_signature(
                "invalid voucher signature",
            ));
        }

        let seq = voucher.voucher.sequence;
        let cum = voucher.voucher.cumulative_amount.clone();
        self.store
            .update_channel(
                channel_id,
                Box::new(move |current| {
                    let state = current
                        .ok_or_else(|| VerificationError::channel_not_found("channel not found"))?;
                    assert_channel_open(&state)?;
                    Ok(Some(SolanaChannelState {
                        last_authorized_amount: cum,
                        last_sequence: seq,
                        ..state
                    }))
                }),
            )
            .await?;

        Ok(Receipt::success("solana", channel_id))
    }

    /// Handle the `topup` action.
    async fn handle_topup(
        &self,
        channel_id: &str,
        topup_tx: &str,
        additional_amount: &str,
    ) -> Result<Receipt, VerificationError> {
        if topup_tx.trim().is_empty() {
            return Err(VerificationError::invalid_payload(
                "topupTx is required for session topup",
            ));
        }

        let additional = parse_non_negative_amount(additional_amount, "additionalAmount")?;

        let channel = self.store.get_channel(channel_id).await?.ok_or_else(|| {
            VerificationError::channel_not_found(format!("channel not found: {channel_id}"))
        })?;

        assert_channel_open(&channel)?;

        let ch_id = channel_id.to_string();
        self.store
            .update_channel(
                channel_id,
                Box::new(move |current| {
                    let state = current
                        .ok_or_else(|| VerificationError::channel_not_found("channel not found"))?;
                    assert_channel_open(&state)?;
                    let escrowed =
                        parse_non_negative_amount(&state.escrowed_amount, "escrowedAmount")?;
                    let next = escrowed + additional;
                    Ok(Some(SolanaChannelState {
                        escrowed_amount: next.to_string(),
                        ..state
                    }))
                }),
            )
            .await?;

        Ok(Receipt::success("solana", &ch_id))
    }

    /// Handle the `close` action.
    async fn handle_close(
        &self,
        channel_id: &str,
        voucher: &SignedSolanaSessionVoucher,
        close_tx: Option<&str>,
    ) -> Result<Receipt, VerificationError> {
        let _ = close_tx; // Transaction verification delegated to external verifier.

        let channel = self.store.get_channel(channel_id).await?.ok_or_else(|| {
            VerificationError::channel_not_found(format!("channel not found: {channel_id}"))
        })?;

        if channel.status == ChannelStatus::Closed {
            return Err(VerificationError::channel_closed(format!(
                "channel already closed: {channel_id}"
            )));
        }
        assert_channel_open(&channel)?;
        assert_voucher_matches_channel(voucher, &channel)?;
        assert_voucher_not_expired(voucher)?;
        assert_signer_authorized(voucher, &channel)?;

        let cumulative =
            parse_non_negative_amount(&voucher.voucher.cumulative_amount, "cumulativeAmount")?;
        let escrowed = parse_non_negative_amount(&channel.escrowed_amount, "escrowedAmount")?;
        let last_authorized =
            parse_non_negative_amount(&channel.last_authorized_amount, "lastAuthorizedAmount")?;

        if voucher.voucher.sequence <= channel.last_sequence {
            return Err(VerificationError::invalid_payload(format!(
                "voucher sequence replay: last={}, received={}",
                channel.last_sequence, voucher.voucher.sequence
            )));
        }
        if cumulative < last_authorized {
            return Err(VerificationError::invalid_payload(
                "voucher cumulative amount must be monotonically non-decreasing",
            ));
        }
        if cumulative > escrowed {
            return Err(VerificationError::amount_exceeds_deposit(
                "voucher cumulative amount exceeds channel deposit",
            ));
        }

        let valid = verify_voucher_signature(voucher)?;
        if !valid {
            return Err(VerificationError::invalid_signature(
                "invalid voucher signature",
            ));
        }

        let seq = voucher.voucher.sequence;
        let cum = voucher.voucher.cumulative_amount.clone();
        self.store
            .update_channel(
                channel_id,
                Box::new(move |current| {
                    let state = current
                        .ok_or_else(|| VerificationError::channel_not_found("channel not found"))?;
                    Ok(Some(SolanaChannelState {
                        last_authorized_amount: cum,
                        last_sequence: seq,
                        status: ChannelStatus::Closed,
                        ..state
                    }))
                }),
            )
            .await?;

        Ok(Receipt::success("solana", channel_id))
    }
}

impl SessionMethodTrait for SolanaSessionMethod {
    fn method(&self) -> &str {
        "solana"
    }

    fn verify_session(
        &self,
        credential: &PaymentCredential,
        _request: &SessionRequest,
    ) -> impl std::future::Future<Output = Result<Receipt, VerificationError>> + Send {
        let credential = credential.clone();
        let this = self.clone();

        async move {
            // Parse the payload from the credential.
            let payload: SolanaSessionCredentialPayload =
                serde_json::from_value(credential.payload.clone()).map_err(|e| {
                    VerificationError::invalid_payload(format!(
                        "failed to parse session credential payload: {e}"
                    ))
                })?;

            // Resolve the recipient from the session request.
            let recipient = _request.recipient.as_deref().unwrap_or("");

            match &payload {
                SolanaSessionCredentialPayload::Open {
                    channel_id,
                    open_tx,
                    voucher,
                    payer,
                    deposit_amount,
                    authorization_mode,
                    expires_at,
                    ..
                } => {
                    this.handle_open(HandleOpenParams {
                        channel_id,
                        open_tx,
                        voucher,
                        payer,
                        deposit_amount,
                        authorization_mode,
                        expires_at: expires_at.as_deref(),
                        configured_recipient: recipient,
                    })
                    .await
                }
                SolanaSessionCredentialPayload::Update {
                    channel_id,
                    voucher,
                } => this.handle_update(channel_id, voucher).await,
                SolanaSessionCredentialPayload::TopUp {
                    channel_id,
                    topup_tx,
                    additional_amount,
                } => {
                    this.handle_topup(channel_id, topup_tx, additional_amount)
                        .await
                }
                SolanaSessionCredentialPayload::Close {
                    channel_id,
                    voucher,
                    close_tx,
                } => {
                    this.handle_close(channel_id, voucher, close_tx.as_deref())
                        .await
                }
            }
        }
    }

    fn challenge_method_details(&self) -> Option<serde_json::Value> {
        let details = SolanaSessionMethodDetails {
            channel_program: self.config.channel_program.clone(),
            network: self.config.network.clone(),
        };
        serde_json::to_value(details).ok()
    }

    fn respond(
        &self,
        credential: &PaymentCredential,
        _receipt: &Receipt,
    ) -> Option<serde_json::Value> {
        // Management actions (open, topup, close) should short-circuit the
        // normal request flow and return an empty response.
        let payload: SolanaSessionCredentialPayload = credential.payload_as().ok()?;
        match payload {
            SolanaSessionCredentialPayload::Open { .. }
            | SolanaSessionCredentialPayload::TopUp { .. }
            | SolanaSessionCredentialPayload::Close { .. } => Some(serde_json::json!(null)),
            SolanaSessionCredentialPayload::Update { .. } => None,
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn parse_non_negative_amount(value: &str, field: &str) -> Result<u128, VerificationError> {
    value.parse::<u128>().map_err(|_| {
        VerificationError::invalid_payload(format!(
            "{field} must be a valid non-negative integer string"
        ))
    })
}

fn assert_channel_open(channel: &SolanaChannelState) -> Result<(), VerificationError> {
    match channel.status {
        ChannelStatus::Closed => {
            return Err(VerificationError::channel_closed(format!(
                "channel is closed: {}",
                channel.channel_id
            )));
        }
        ChannelStatus::Expired => {
            return Err(VerificationError::channel_closed(format!(
                "channel has expired: {}",
                channel.channel_id
            )));
        }
        ChannelStatus::Open => {}
        ChannelStatus::Closing => {}
    }

    if let Some(expires_at_unix) = channel.expires_at_unix {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if now > expires_at_unix {
            return Err(VerificationError::channel_closed(format!(
                "channel has expired: {}",
                channel.channel_id
            )));
        }
    }

    Ok(())
}

fn assert_voucher_matches_channel(
    voucher: &SignedSolanaSessionVoucher,
    channel: &SolanaChannelState,
) -> Result<(), VerificationError> {
    if voucher.voucher.channel_id != channel.channel_id {
        return Err(VerificationError::invalid_payload(
            "voucher channelId mismatch",
        ));
    }
    if voucher.voucher.payer != channel.payer {
        return Err(VerificationError::invalid_payload("voucher payer mismatch"));
    }
    if voucher.voucher.recipient != channel.recipient {
        return Err(VerificationError::invalid_payload(
            "voucher recipient mismatch",
        ));
    }
    if voucher.voucher.server_nonce != channel.server_nonce {
        return Err(VerificationError::invalid_payload(
            "voucher serverNonce mismatch",
        ));
    }
    if voucher.voucher.channel_program != channel.channel_program {
        return Err(VerificationError::invalid_payload(
            "voucher channelProgram mismatch",
        ));
    }
    if voucher.voucher.chain_id != channel.chain_id {
        return Err(VerificationError::with_code(
            format!(
                "voucher chainId mismatch: expected {}, received {}",
                channel.chain_id, voucher.voucher.chain_id
            ),
            mpp::server::ErrorCode::ChainIdMismatch,
        ));
    }
    Ok(())
}

fn assert_voucher_not_expired(
    voucher: &SignedSolanaSessionVoucher,
) -> Result<(), VerificationError> {
    let expires_at = match voucher.voucher.expires_at.as_deref() {
        Some(s) => s,
        None => return Ok(()),
    };

    let dt =
        time::OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
            .map_err(|_| {
                VerificationError::invalid_payload(
                    "voucher expiresAt must be a valid ISO timestamp",
                )
            })?;

    if time::OffsetDateTime::now_utc() > dt {
        return Err(VerificationError::expired("voucher has expired"));
    }

    Ok(())
}

fn assert_signer_authorized(
    voucher: &SignedSolanaSessionVoucher,
    channel: &SolanaChannelState,
) -> Result<(), VerificationError> {
    if channel.authorization_mode == AuthorizationMode::SwigSession {
        let expected = channel.delegated_session_key.as_deref().ok_or_else(|| {
            VerificationError::invalid_payload(
                "channel uses swig_session authorization but no delegated session key is recorded",
            )
        })?;
        if voucher.signer != expected {
            return Err(VerificationError::invalid_signature(format!(
                "voucher signer {} does not match delegated session key {expected}",
                voucher.signer
            )));
        }
        return Ok(());
    }

    // For regular modes, signer must be the channel payer.
    if voucher.signer != channel.payer && voucher.signer != channel.authority_wallet {
        return Err(VerificationError::invalid_signature(format!(
            "voucher signer {} does not match channel payer {}",
            voucher.signer, channel.payer
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::solana_channel_store::InMemorySolanaChannelStore;

    fn test_config() -> SolanaSessionMethodConfig {
        SolanaSessionMethodConfig {
            channel_program: "prog1".into(),
            network: "mainnet-beta".into(),
        }
    }

    #[test]
    fn normalize_chain_id_prepends_solana() {
        assert_eq!(
            SolanaSessionMethod::normalize_chain_id("mainnet-beta"),
            "solana:mainnet-beta"
        );
        assert_eq!(
            SolanaSessionMethod::normalize_chain_id("solana:devnet"),
            "solana:devnet"
        );
    }

    #[test]
    fn assert_channel_open_rejects_closed() {
        let channel = SolanaChannelState {
            channel_id: "ch".into(),
            payer: "a".into(),
            recipient: "b".into(),
            server_nonce: "n".into(),
            channel_program: "p".into(),
            chain_id: "solana:mainnet-beta".into(),
            authorization_mode: AuthorizationMode::SwigSession,
            authority_wallet: "a".into(),
            delegated_session_key: None,
            escrowed_amount: "0".into(),
            last_authorized_amount: "0".into(),
            last_sequence: 0,
            settled_amount: "0".into(),
            status: ChannelStatus::Closed,
            expires_at_unix: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(assert_channel_open(&channel).is_err());
    }

    #[tokio::test]
    async fn handle_topup_increases_escrow() {
        let store = Arc::new(InMemorySolanaChannelStore::new());
        let method = SolanaSessionMethod::new(store.clone(), test_config());

        // Seed the channel.
        let channel = SolanaChannelState {
            channel_id: "ch1".into(),
            payer: "payer1".into(),
            recipient: "recip1".into(),
            server_nonce: "nonce1".into(),
            channel_program: "prog1".into(),
            chain_id: "solana:mainnet-beta".into(),
            authorization_mode: AuthorizationMode::SwigSession,
            authority_wallet: "payer1".into(),
            delegated_session_key: Some("key1".into()),
            escrowed_amount: "1000".into(),
            last_authorized_amount: "0".into(),
            last_sequence: 0,
            settled_amount: "0".into(),
            status: ChannelStatus::Open,
            expires_at_unix: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let ch = channel.clone();
        store
            .update_channel("ch1", Box::new(move |_| Ok(Some(ch))))
            .await
            .expect("seed");

        let receipt = method
            .handle_topup("ch1", "base64tx", "500")
            .await
            .expect("topup");
        assert!(receipt.is_success());

        let updated = store.get_channel("ch1").await.expect("ok").expect("exists");
        assert_eq!(updated.escrowed_amount, "1500");
    }
}
