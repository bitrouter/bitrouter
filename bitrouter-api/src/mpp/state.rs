use std::sync::Arc;

use bitrouter_config::TempoMppConfig;
use mpp::server::{
    Mpp, SessionChallengeOptions, SessionMethodConfig, TempoChargeMethod, TempoConfig,
    TempoProvider, TempoSessionMethod,
};

type TempoMpp = Mpp<TempoChargeMethod<TempoProvider>, TempoSessionMethod<TempoProvider>>;

/// Server-side MPP (Machine Payment Protocol) state.
///
/// Holds a configured `Mpp` instance with session support for the Tempo chain.
/// Constructed from [`TempoMppConfig`] during server startup.
pub struct MppState {
    mpp: TempoMpp,
}

impl MppState {
    /// Create an `MppState` from Tempo configuration.
    ///
    /// Initializes both charge and session methods backed by the Tempo chain.
    pub fn from_tempo_config(
        tempo: &TempoMppConfig,
        realm: Option<&str>,
        secret_key: Option<&str>,
    ) -> Result<Self, mpp::MppError> {
        let rpc_url = tempo.rpc_url.as_deref().unwrap_or("https://rpc.tempo.xyz");

        // Build the Mpp instance via the simple API (binds currency + recipient).
        let mut builder = mpp::server::tempo(TempoConfig {
            recipient: &tempo.recipient,
        })
        .rpc_url(rpc_url);

        if let Some(r) = realm {
            builder = builder.realm(r);
        }
        if let Some(s) = secret_key {
            builder = builder.secret_key(s);
        }
        if let Some(ref c) = tempo.currency {
            builder = builder.currency(c);
        }
        if tempo.fee_payer {
            builder = builder.fee_payer(true);
        }

        let mpp = Mpp::create(builder)?;

        // Create a separate provider for the session method.
        let session_provider = mpp::server::tempo_provider(rpc_url)?;
        let store = Arc::new(mpp::server::SessionChannelStore::new());

        let escrow = tempo
            .escrow_contract
            .parse()
            .map_err(|e| mpp::MppError::InvalidConfig(format!("invalid escrow_contract: {e}")))?;

        let chain_id = if rpc_url.contains("moderato") {
            42431
        } else {
            4217
        };

        let session_config = SessionMethodConfig {
            escrow_contract: escrow,
            chain_id,
            min_voucher_delta: 0,
        };

        let session_method = TempoSessionMethod::new(session_provider, store, session_config);
        let mpp = mpp.with_session_method(session_method);

        Ok(Self { mpp })
    }

    /// Issue a session challenge with per-unit pricing details.
    pub fn session_challenge(
        &self,
        amount: &str,
        options: SessionChallengeOptions<'_>,
    ) -> Result<mpp::PaymentChallenge, mpp::MppError> {
        let currency = self
            .mpp
            .currency()
            .ok_or_else(|| mpp::MppError::InvalidConfig("currency not configured".into()))?;
        let recipient = self
            .mpp
            .recipient()
            .ok_or_else(|| mpp::MppError::InvalidConfig("recipient not configured".into()))?;
        self.mpp
            .session_challenge_with_details(amount, currency, recipient, options)
    }

    /// Verify an incoming session credential.
    pub async fn verify_session(
        &self,
        credential: &mpp::PaymentCredential,
    ) -> Result<mpp::server::SessionVerifyResult, mpp::server::VerificationError> {
        self.mpp.verify_session(credential).await
    }

    /// The server's realm.
    pub fn realm(&self) -> &str {
        self.mpp.realm()
    }
}
