//! In-memory channel store for Solana session payments.
//!
//! Provides atomic read-modify-write semantics for channel state,
//! mirroring the Tempo `SessionChannelStore` pattern and the TS SDK's
//! `ChannelStore.ts`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use mpp::server::VerificationError;
use tokio::sync::RwLock;

use super::solana_types::SolanaChannelState;

/// Type alias for the channel updater callback.
pub type ChannelUpdater = Box<
    dyn FnOnce(Option<SolanaChannelState>) -> Result<Option<SolanaChannelState>, VerificationError>
        + Send,
>;

/// Trait for Solana channel state persistence.
///
/// Implementations must provide atomic read-modify-write semantics for
/// `update_channel`. Object-safe so it can be used as `Arc<dyn SolanaChannelStore>`.
pub trait SolanaChannelStore: Send + Sync {
    /// Get the current state of a channel.
    fn get_channel(
        &self,
        channel_id: &str,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<SolanaChannelState>, VerificationError>> + Send + '_>,
    >;

    /// Atomically update a channel's state.
    ///
    /// The updater receives the current state (or `None` for new channels)
    /// and returns the next state (or `None` to delete).
    fn update_channel(
        &self,
        channel_id: &str,
        updater: ChannelUpdater,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<SolanaChannelState>, VerificationError>> + Send + '_>,
    >;
}

/// In-memory channel store backed by a `RwLock<HashMap>`.
pub struct InMemorySolanaChannelStore {
    channels: RwLock<HashMap<String, SolanaChannelState>>,
}

impl Default for InMemorySolanaChannelStore {
    fn default() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }
}

impl InMemorySolanaChannelStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SolanaChannelStore for InMemorySolanaChannelStore {
    fn get_channel(
        &self,
        channel_id: &str,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<SolanaChannelState>, VerificationError>> + Send + '_>,
    > {
        let channel_id = channel_id.to_string();
        Box::pin(async move {
            let channels = self.channels.read().await;
            Ok(channels.get(&channel_id).cloned())
        })
    }

    fn update_channel(
        &self,
        channel_id: &str,
        updater: ChannelUpdater,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<SolanaChannelState>, VerificationError>> + Send + '_>,
    > {
        let channel_id = channel_id.to_string();
        Box::pin(async move {
            let mut channels = self.channels.write().await;
            let current = channels.get(&channel_id).cloned();
            let next = updater(current)?;
            match next {
                Some(ref state) => {
                    channels.insert(channel_id, state.clone());
                }
                None => {
                    channels.remove(&channel_id);
                }
            }
            Ok(next)
        })
    }
}

/// Atomically deduct `amount` from a channel's available balance.
///
/// Available balance = `last_authorized_amount - settled_amount`.
/// Returns the updated channel state on success.
pub async fn deduct_from_channel(
    store: &dyn SolanaChannelStore,
    channel_id: &str,
    amount: u128,
) -> Result<SolanaChannelState, VerificationError> {
    let result = store
        .update_channel(
            channel_id,
            Box::new(move |current| {
                let state = current
                    .ok_or_else(|| VerificationError::channel_not_found("channel not found"))?;

                let authorized: u128 = state.last_authorized_amount.parse().map_err(|_| {
                    VerificationError::invalid_payload("invalid last_authorized_amount")
                })?;
                let settled: u128 = state
                    .settled_amount
                    .parse()
                    .map_err(|_| VerificationError::invalid_payload("invalid settled_amount"))?;

                let available = authorized.saturating_sub(settled);
                if available < amount {
                    return Err(VerificationError::insufficient_balance(format!(
                        "requested {amount}, available {available}",
                    )));
                }

                Ok(Some(SolanaChannelState {
                    settled_amount: (settled + amount).to_string(),
                    ..state
                }))
            }),
        )
        .await?;

    result.ok_or_else(|| VerificationError::channel_not_found("channel not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::solana_types::{AuthorizationMode, ChannelStatus};

    fn test_channel(id: &str) -> SolanaChannelState {
        SolanaChannelState {
            channel_id: id.into(),
            payer: "alice".into(),
            recipient: "bob".into(),
            server_nonce: "nonce".into(),
            channel_program: "prog".into(),
            chain_id: "solana:mainnet-beta".into(),
            authorization_mode: AuthorizationMode::SwigSession,
            authority_wallet: "alice".into(),
            delegated_session_key: None,
            escrowed_amount: "1000".into(),
            last_authorized_amount: "500".into(),
            last_sequence: 1,
            settled_amount: "100".into(),
            status: ChannelStatus::Open,
            expires_at_unix: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn get_missing_channel_returns_none() {
        let store = InMemorySolanaChannelStore::new();
        let result = store.get_channel("missing").await.expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn update_creates_new_channel() {
        let store = InMemorySolanaChannelStore::new();
        let channel = test_channel("ch1");
        let ch = channel.clone();
        let result = store
            .update_channel("ch1", Box::new(move |_| Ok(Some(ch))))
            .await
            .expect("no error");
        assert!(result.is_some());

        let fetched = store.get_channel("ch1").await.expect("no error");
        assert_eq!(fetched.expect("exists").channel_id, channel.channel_id);
    }

    #[tokio::test]
    async fn deduct_succeeds_with_available_balance() {
        let store = InMemorySolanaChannelStore::new();
        let channel = test_channel("ch1");
        let ch = channel.clone();
        store
            .update_channel("ch1", Box::new(move |_| Ok(Some(ch))))
            .await
            .expect("no error");

        // authorized=500, settled=100, available=400. Deduct 200.
        let updated = deduct_from_channel(&store, "ch1", 200).await.expect("ok");
        assert_eq!(updated.settled_amount, "300");
    }

    #[tokio::test]
    async fn deduct_fails_with_insufficient_balance() {
        let store = InMemorySolanaChannelStore::new();
        let channel = test_channel("ch1");
        let ch = channel.clone();
        store
            .update_channel("ch1", Box::new(move |_| Ok(Some(ch))))
            .await
            .expect("no error");

        // available=400. Deduct 500 → should fail.
        let result = deduct_from_channel(&store, "ch1", 500).await;
        assert!(result.is_err());
    }
}
