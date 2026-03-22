use std::sync::Arc;

#[cfg(feature = "mpp-tempo")]
use bitrouter_config::TempoMppConfig;
#[cfg(feature = "mpp-tempo")]
use mpp::server::{
    Mpp, SessionMethodConfig, TempoChargeMethod, TempoConfig, TempoProvider, TempoSessionMethod,
};

use mpp::server::SessionChallengeOptions;

#[cfg(feature = "mpp-tempo")]
type TempoMpp = Mpp<TempoChargeMethod<TempoProvider>, TempoSessionMethod<TempoProvider>>;

/// Server-side MPP (Machine Payment Protocol) state.
///
/// Holds a configured backend for session payment verification.
/// Supports both Tempo and Solana networks, selected at construction time.
pub struct MppState {
    inner: MppBackend,
}

enum MppBackend {
    #[cfg(feature = "mpp-tempo")]
    Tempo(TempoMpp),
    #[cfg(feature = "mpp-solana")]
    Solana(SolanaState),
}

#[cfg(feature = "mpp-solana")]
struct SolanaState {
    realm: String,
    secret_key: String,
    currency: String,
    recipient: String,
    session_method: super::solana_session_method::SolanaSessionMethod,
}

impl MppState {
    /// Create an `MppState` from Tempo configuration.
    ///
    /// Initializes both charge and session methods backed by the Tempo chain.
    #[cfg(feature = "mpp-tempo")]
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

        Ok(Self {
            inner: MppBackend::Tempo(mpp),
        })
    }

    /// Create an `MppState` for Solana session payments.
    #[cfg(feature = "mpp-solana")]
    pub fn from_solana_config(
        solana: &bitrouter_config::SolanaMppConfig,
        realm: Option<&str>,
        secret_key: Option<&str>,
    ) -> Result<Self, mpp::MppError> {
        use super::solana_channel_store::InMemorySolanaChannelStore;
        use super::solana_session_method::{SolanaSessionMethod, SolanaSessionMethodConfig};

        let realm = realm
            .map(|s| s.to_string())
            .unwrap_or_else(|| "MPP Payment".to_string());

        let secret_key = secret_key
            .map(|s| s.to_string())
            .or_else(|| std::env::var("MPP_SECRET_KEY").ok())
            .and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
            .ok_or_else(|| {
                mpp::MppError::InvalidConfig(
                    "Missing secret key. Set MPP_SECRET_KEY or pass secret_key.".into(),
                )
            })?;

        let store = Arc::new(InMemorySolanaChannelStore::new());
        let config = SolanaSessionMethodConfig {
            channel_program: solana.channel_program.clone(),
            network: solana.network.clone(),
        };
        let session_method = SolanaSessionMethod::new(store, config);

        // Solana payments use SOL / lamports by default.
        let currency = "SOL".to_string();

        Ok(Self {
            inner: MppBackend::Solana(SolanaState {
                realm,
                secret_key,
                currency,
                recipient: solana.recipient.clone(),
                session_method,
            }),
        })
    }

    /// Issue a session challenge with per-unit pricing details.
    pub fn session_challenge(
        &self,
        amount: &str,
        options: SessionChallengeOptions<'_>,
    ) -> Result<mpp::PaymentChallenge, mpp::MppError> {
        match &self.inner {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo(mpp) => {
                let currency = mpp.currency().ok_or_else(|| {
                    mpp::MppError::InvalidConfig("currency not configured".into())
                })?;
                let recipient = mpp.recipient().ok_or_else(|| {
                    mpp::MppError::InvalidConfig("recipient not configured".into())
                })?;
                mpp.session_challenge_with_details(amount, currency, recipient, options)
            }
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => solana_session_challenge(state, amount, options),
        }
    }

    /// Verify an incoming session credential.
    pub async fn verify_session(
        &self,
        credential: &mpp::PaymentCredential,
    ) -> Result<mpp::server::SessionVerifyResult, mpp::server::VerificationError> {
        match &self.inner {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo(mpp) => mpp.verify_session(credential).await,
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => solana_verify_session(state, credential).await,
        }
    }

    /// The server's realm.
    pub fn realm(&self) -> &str {
        match &self.inner {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo(mpp) => mpp.realm(),
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => &state.realm,
        }
    }
}

// ── Solana helpers ───────────────────────────────────────────────────

#[cfg(feature = "mpp-solana")]
fn solana_session_challenge(
    state: &SolanaState,
    amount: &str,
    options: SessionChallengeOptions<'_>,
) -> Result<mpp::PaymentChallenge, mpp::MppError> {
    use mpp::protocol::traits::SessionMethod as _;

    let method_details = state.session_method.challenge_method_details();

    let request = mpp::SessionRequest {
        amount: amount.to_string(),
        unit_type: options.unit_type.map(|s| s.to_string()),
        currency: state.currency.clone(),
        recipient: Some(state.recipient.clone()),
        suggested_deposit: options.suggested_deposit.map(|s| s.to_string()),
        method_details,
        ..Default::default()
    };

    let encoded = mpp::Base64UrlJson::from_typed(&request)?;

    let id = mpp::compute_challenge_id(
        &state.secret_key,
        &state.realm,
        "solana",
        "session",
        encoded.raw(),
        options.expires,
        None,
        None,
    );

    Ok(mpp::PaymentChallenge {
        id,
        realm: state.realm.clone(),
        method: "solana".into(),
        intent: "session".into(),
        request: encoded,
        expires: options.expires.map(|s| s.to_string()),
        description: options.description.map(|s| s.to_string()),
        digest: None,
        opaque: None,
    })
}

#[cfg(feature = "mpp-solana")]
async fn solana_verify_session(
    state: &SolanaState,
    credential: &mpp::PaymentCredential,
) -> Result<mpp::server::SessionVerifyResult, mpp::server::VerificationError> {
    use mpp::protocol::traits::SessionMethod as _;
    use mpp::server::VerificationError;

    // HMAC check.
    let expected_id = mpp::compute_challenge_id(
        &state.secret_key,
        &state.realm,
        credential.challenge.method.as_str(),
        credential.challenge.intent.as_str(),
        credential.challenge.request.raw(),
        credential.challenge.expires.as_deref(),
        credential.challenge.digest.as_deref(),
        credential.challenge.opaque.as_ref().map(|o| o.raw()),
    );
    if credential.challenge.id != expected_id {
        return Err(VerificationError::with_code(
            "Challenge ID mismatch - not issued by this server",
            mpp::server::ErrorCode::CredentialMismatch,
        ));
    }

    // Expiry check.
    if let Some(ref expires) = credential.challenge.expires {
        if let Ok(expires_at) =
            time::OffsetDateTime::parse(expires, &time::format_description::well_known::Rfc3339)
        {
            if expires_at <= time::OffsetDateTime::now_utc() {
                return Err(VerificationError::expired(format!(
                    "Challenge expired at {expires}"
                )));
            }
        } else {
            return Err(VerificationError::new(
                "Invalid expires timestamp in challenge",
            ));
        }
    }

    // Decode session request.
    let request: mpp::SessionRequest =
        credential.challenge.request.decode().map_err(|e| {
            VerificationError::new(format!("Failed to decode session request: {e}"))
        })?;

    let receipt = state
        .session_method
        .verify_session(credential, &request)
        .await?;

    let management_response = state.session_method.respond(credential, &receipt);

    Ok(mpp::server::SessionVerifyResult {
        receipt,
        management_response,
    })
}
