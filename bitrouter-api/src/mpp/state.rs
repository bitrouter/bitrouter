use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "mpp-tempo")]
use bitrouter_config::TempoMppConfig;
#[cfg(feature = "mpp-tempo")]
use mpp::server::{
    Mpp, SessionMethodConfig, TempoChargeMethod, TempoConfig, TempoProvider, TempoSessionMethod,
};

use mpp::server::SessionChallengeOptions;

#[cfg(feature = "mpp-tempo")]
use mpp::protocol::methods::tempo::session_method::ChannelStore as TempoChannelStore;

#[cfg(feature = "mpp-tempo")]
type TempoMpp = Mpp<TempoChargeMethod<TempoProvider>, TempoSessionMethod<TempoProvider>>;

/// Server-side MPP (Machine Payment Protocol) state.
///
/// Holds one or more configured payment backends, keyed by CAIP-2 chain
/// identifier (e.g. `"eip155:4217"` for Tempo, `"solana"` for Solana).
/// A request's JWT `chain` claim selects which backend handles payment.
pub struct MppState {
    backends: HashMap<String, MppBackend>,
    realm: String,
}

enum MppBackend {
    #[cfg(feature = "mpp-tempo")]
    Tempo {
        mpp: TempoMpp,
        store: Arc<dyn TempoChannelStore>,
    },
    #[cfg(feature = "mpp-solana")]
    Solana(SolanaState),
}

impl MppBackend {
    /// The protocol-level payment method name (e.g. `"tempo"`, `"solana"`).
    ///
    /// This is the value that appears in a 402 challenge's `method` field and
    /// may differ from the CAIP-2 key used to register the backend.
    fn method_name(&self) -> &'static str {
        match self {
            #[cfg(feature = "mpp-tempo")]
            Self::Tempo { .. } => "tempo",
            #[cfg(feature = "mpp-solana")]
            Self::Solana(_) => "solana",
        }
    }
}

#[cfg(feature = "mpp-solana")]
struct SolanaState {
    realm: String,
    secret_key: String,
    asset: bitrouter_config::config::SolanaAssetConfig,
    recipient: String,
    session_method: super::solana_session_method::SolanaSessionMethod,
    suggested_deposit: Option<String>,
}

impl MppState {
    /// Create an empty `MppState` with the given realm.
    pub fn new(realm: &str) -> Self {
        Self {
            backends: HashMap::new(),
            realm: realm.to_string(),
        }
    }

    /// Add a Tempo backend for the given chain IDs.
    ///
    /// Uses the default in-memory session channel store.
    /// Typically called with `"eip155:4217"` (mainnet) or `"eip155:42431"` (testnet).
    #[cfg(feature = "mpp-tempo")]
    pub fn add_tempo(
        &mut self,
        tempo: &TempoMppConfig,
        secret_key: Option<&str>,
    ) -> Result<(), mpp::MppError> {
        let store: Arc<dyn TempoChannelStore> = Arc::new(mpp::server::SessionChannelStore::new());
        self.add_tempo_with_store(tempo, secret_key, store)
    }

    /// Add a Tempo backend with a caller-provided channel store.
    ///
    /// This allows injecting a custom (e.g. database-backed) store instead of
    /// the default in-memory store.
    #[cfg(feature = "mpp-tempo")]
    pub fn add_tempo_with_store(
        &mut self,
        tempo: &TempoMppConfig,
        secret_key: Option<&str>,
        store: Arc<dyn TempoChannelStore>,
    ) -> Result<(), mpp::MppError> {
        let rpc_url = tempo.rpc_url.as_deref().unwrap_or("https://rpc.tempo.xyz");

        let mut builder = mpp::server::tempo(TempoConfig {
            recipient: &tempo.recipient,
        })
        .rpc_url(rpc_url)
        .realm(&self.realm);

        if let Some(s) = secret_key {
            builder = builder.secret_key(s);
        }
        if let Some(ref c) = tempo.currency {
            builder = builder.currency(c);
        }
        if tempo.fee_payer {
            builder = builder.fee_payer(true);
        }

        let mpp_instance = Mpp::create(builder)?;

        let session_provider = mpp::server::tempo_provider(rpc_url)?;

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

        let session_method =
            TempoSessionMethod::new(session_provider, store.clone(), session_config);
        let mpp_instance = mpp_instance.with_session_method(session_method);

        let caip2 = format!("eip155:{chain_id}");
        self.backends.insert(
            caip2,
            MppBackend::Tempo {
                mpp: mpp_instance,
                store,
            },
        );
        Ok(())
    }

    /// Add a Solana backend keyed by `"solana"`.
    ///
    /// Uses the default in-memory channel store.
    #[cfg(feature = "mpp-solana")]
    pub fn add_solana(
        &mut self,
        solana: &bitrouter_config::SolanaMppConfig,
        secret_key: Option<&str>,
    ) -> Result<(), mpp::MppError> {
        use super::solana_channel_store::InMemorySolanaChannelStore;

        let store: Arc<dyn super::solana_channel_store::SolanaChannelStore> =
            Arc::new(InMemorySolanaChannelStore::new());
        self.add_solana_with_store(solana, secret_key, store)
    }

    /// Add a Solana backend with a caller-provided channel store.
    ///
    /// This allows injecting a custom (e.g. database-backed) store instead of
    /// the default in-memory store.
    #[cfg(feature = "mpp-solana")]
    pub fn add_solana_with_store(
        &mut self,
        solana: &bitrouter_config::SolanaMppConfig,
        secret_key: Option<&str>,
        store: Arc<dyn super::solana_channel_store::SolanaChannelStore>,
    ) -> Result<(), mpp::MppError> {
        use super::solana_session_method::{SolanaSessionMethod, SolanaSessionMethodConfig};

        let secret_key = secret_key
            .map(|s| s.to_string())
            .or_else(|| std::env::var("MPP_SECRET_KEY").ok())
            .and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
            .ok_or_else(|| {
                mpp::MppError::InvalidConfig(
                    "Missing secret key. Set MPP_SECRET_KEY or pass secret_key.".into(),
                )
            })?;

        let config = SolanaSessionMethodConfig {
            channel_program: solana.channel_program.clone(),
            network: solana.network.clone(),
        };
        let session_method = SolanaSessionMethod::new(store, config);

        self.backends.insert(
            "solana".to_string(),
            MppBackend::Solana(SolanaState {
                realm: self.realm.clone(),
                secret_key,
                asset: solana.asset.clone(),
                recipient: solana.recipient.clone(),
                session_method,
                suggested_deposit: solana.suggested_deposit.clone(),
            }),
        );
        Ok(())
    }

    /// Look up the backend that handles the given CAIP-2 chain identifier.
    ///
    /// Supports exact match (`"eip155:4217"`), namespace-prefix match
    /// (`"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"` matches key `"solana"`),
    /// and payment-method-name match (`"tempo"` matches a Tempo backend
    /// regardless of its CAIP-2 registration key).
    fn backend_for_chain(&self, chain: &str) -> Option<(&String, &MppBackend)> {
        // Exact match first.
        if let Some((key, backend)) = self.backends.get_key_value(chain) {
            return Some((key, backend));
        }
        // Namespace-prefix match (e.g. "solana:xxx" matches key "solana").
        let namespace = chain.split_once(':').map(|(ns, _)| ns).unwrap_or(chain);
        for (key, backend) in &self.backends {
            if key == namespace {
                return Some((key, backend));
            }
        }
        // Payment method name match (e.g. "tempo" matches a Tempo backend
        // even when it is registered under "eip155:42431").
        for (key, backend) in &self.backends {
            if backend.method_name() == chain {
                return Some((key, backend));
            }
        }
        None
    }

    /// Returns `true` if at least one backend is configured.
    pub fn is_configured(&self) -> bool {
        !self.backends.is_empty()
    }

    /// Issue a session challenge for a specific backend selected by chain.
    ///
    /// If `chain` is `None`, returns challenges from all backends.
    pub fn session_challenge(
        &self,
        chain: Option<&str>,
        amount: &str,
        options: SessionChallengeOptions<'_>,
    ) -> Result<mpp::PaymentChallenge, mpp::MppError> {
        let (_key, backend) = match chain {
            Some(c) => self.backend_for_chain(c).ok_or_else(|| {
                mpp::MppError::InvalidConfig(format!("no backend for chain: {c}"))
            })?,
            None => {
                // No chain specified — use first available backend.
                self.backends.iter().next().ok_or_else(|| {
                    mpp::MppError::InvalidConfig("no MPP backends configured".into())
                })?
            }
        };
        backend_session_challenge(backend, amount, options)
    }

    /// Issue session challenges from **all** configured backends.
    ///
    /// Used when the caller's chain is unknown and we want to present
    /// all available payment options in the 402 response.
    pub fn all_session_challenges(
        &self,
        amount: &str,
        options: SessionChallengeOptions<'_>,
    ) -> Vec<mpp::PaymentChallenge> {
        self.backends
            .values()
            .filter_map(|backend| {
                backend_session_challenge(
                    backend,
                    amount,
                    SessionChallengeOptions {
                        unit_type: options.unit_type,
                        suggested_deposit: options.suggested_deposit,
                        fee_payer: options.fee_payer,
                        expires: options.expires,
                        description: options.description,
                    },
                )
                .ok()
            })
            .collect()
    }

    /// Verify an incoming session credential against the matching backend.
    ///
    /// The `method` field of the credential's challenge selects the backend.
    /// Returns the verification result along with the backend key for deduction routing.
    pub async fn verify_session(
        &self,
        credential: &mpp::PaymentCredential,
    ) -> Result<(String, mpp::server::SessionVerifyResult), mpp::server::VerificationError> {
        let method = credential.challenge.method.as_str();
        let (key, backend) = self.backend_for_chain(method).ok_or_else(|| {
            mpp::server::VerificationError::new(format!("no backend for payment method: {method}"))
        })?;
        let key = key.clone();
        let result = backend_verify_session(backend, credential).await?;
        Ok((key, result))
    }

    /// Deduct `amount` micro-units from the channel in the specified backend.
    ///
    /// Routes to the correct backend's channel store based on `backend_key`.
    pub async fn deduct(
        &self,
        backend_key: &str,
        channel_id: &str,
        amount: u128,
    ) -> Result<(), mpp::server::VerificationError> {
        let (_key, backend) = self.backend_for_chain(backend_key).ok_or_else(|| {
            mpp::server::VerificationError::new(format!("no backend for key: {backend_key}"))
        })?;
        match backend {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo { store, .. } => {
                mpp::protocol::methods::tempo::session_method::deduct_from_channel(
                    &**store, channel_id, amount,
                )
                .await?;
            }
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => {
                super::solana_channel_store::deduct_from_channel(
                    state.session_method.store(),
                    channel_id,
                    amount,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Wait for the next channel update on the given backend.
    ///
    /// Used by metered SSE to pause until a new voucher arrives.
    pub async fn wait_for_update(&self, backend_key: &str, channel_id: &str) {
        let Some((_key, backend)) = self.backend_for_chain(backend_key) else {
            return;
        };
        match backend {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo { store, .. } => {
                store.wait_for_update(channel_id).await;
            }
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => {
                state
                    .session_method
                    .store()
                    .wait_for_update(channel_id)
                    .await;
            }
        }
    }

    /// Retrieve channel balance info for the NeedVoucher event.
    ///
    /// Returns `(settled, authorized, deposit)` in micro-units, or `None`
    /// if the channel is not found.
    pub async fn channel_balance(
        &self,
        backend_key: &str,
        channel_id: &str,
    ) -> Option<(u128, u128, u128)> {
        let (_key, backend) = self.backend_for_chain(backend_key)?;
        match backend {
            #[cfg(feature = "mpp-tempo")]
            MppBackend::Tempo { store, .. } => {
                let ch = store.get_channel(channel_id).await.ok()??;
                Some((ch.spent, ch.highest_voucher_amount, ch.deposit))
            }
            #[cfg(feature = "mpp-solana")]
            MppBackend::Solana(state) => {
                let ch = state
                    .session_method
                    .store()
                    .get_channel(channel_id)
                    .await
                    .ok()??;
                let settled: u128 = ch.settled_amount.parse().ok()?;
                let authorized: u128 = ch.last_authorized_amount.parse().ok()?;
                let deposit: u128 = ch.escrowed_amount.parse().ok()?;
                Some((settled, authorized, deposit))
            }
        }
    }

    /// The server's realm.
    pub fn realm(&self) -> &str {
        &self.realm
    }
}

fn backend_session_challenge(
    backend: &MppBackend,
    amount: &str,
    options: SessionChallengeOptions<'_>,
) -> Result<mpp::PaymentChallenge, mpp::MppError> {
    match backend {
        #[cfg(feature = "mpp-tempo")]
        MppBackend::Tempo { mpp, .. } => {
            let currency = mpp
                .currency()
                .ok_or_else(|| mpp::MppError::InvalidConfig("currency not configured".into()))?;
            let recipient = mpp
                .recipient()
                .ok_or_else(|| mpp::MppError::InvalidConfig("recipient not configured".into()))?;
            mpp.session_challenge_with_details(amount, currency, recipient, options)
        }
        #[cfg(feature = "mpp-solana")]
        MppBackend::Solana(state) => solana_session_challenge(state, amount, options),
    }
}

async fn backend_verify_session(
    backend: &MppBackend,
    credential: &mpp::PaymentCredential,
) -> Result<mpp::server::SessionVerifyResult, mpp::server::VerificationError> {
    match backend {
        #[cfg(feature = "mpp-tempo")]
        MppBackend::Tempo { mpp, .. } => mpp.verify_session(credential).await,
        #[cfg(feature = "mpp-solana")]
        MppBackend::Solana(state) => solana_verify_session(state, credential).await,
    }
}

// ── Solana helpers ───────────────────────────────────────────────────

#[cfg(feature = "mpp-solana")]
fn solana_session_challenge(
    state: &SolanaState,
    _amount: &str,
    options: SessionChallengeOptions<'_>,
) -> Result<mpp::PaymentChallenge, mpp::MppError> {
    use super::solana_types::{SolanaAsset, SolanaSessionChallengeRequest, SolanaSessionDefaults};

    let config = state.session_method.config();

    let deposit = options
        .suggested_deposit
        .map(|d| d.to_string())
        .or_else(|| state.suggested_deposit.clone());

    let request = SolanaSessionChallengeRequest {
        asset: SolanaAsset {
            kind: state.asset.kind.clone(),
            decimals: state.asset.decimals,
            mint: state.asset.mint.clone(),
            symbol: state.asset.symbol.clone(),
        },
        channel_program: config.channel_program.clone(),
        network: Some(config.network.clone()),
        recipient: state.recipient.clone(),
        session_defaults: deposit.map(|d| SolanaSessionDefaults {
            suggested_deposit: Some(d),
        }),
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

    // Decode session request and convert to the SessionRequest expected by the trait.
    let solana_request: super::solana_types::SolanaSessionChallengeRequest =
        credential.challenge.request.decode().map_err(|e| {
            VerificationError::new(format!("Failed to decode session request: {e}"))
        })?;

    let request = mpp::SessionRequest {
        amount: "0".to_string(),
        currency: solana_request.asset.symbol.clone().unwrap_or_default(),
        recipient: Some(solana_request.recipient.clone()),
        ..Default::default()
    };

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
