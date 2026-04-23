use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "payments-tempo")]
use bitrouter_config::TempoMppConfig;
#[cfg(feature = "payments-tempo")]
use mpp::server::{
    Mpp, SessionMethodConfig, TempoChargeMethod, TempoConfig, TempoProvider, TempoSessionMethod,
};

use mpp::server::SessionChallengeOptions;

#[cfg(feature = "payments-tempo")]
use mpp::protocol::methods::tempo::session_method::ChannelStore as TempoChannelStore;

#[cfg(feature = "payments-tempo")]
use mpp::Address;

#[cfg(feature = "payments-tempo")]
type TempoMpp = Mpp<TempoChargeMethod<TempoProvider>, TempoSessionMethod<TempoProvider>>;

/// Wrapper that bridges `Arc<dyn mpp::Signer>` into `impl mpp::Signer`.
///
/// Alloy's `Signer` trait uses `#[auto_impl(&mut, Box)]`, so `Box<dyn Signer>`
/// implements `Signer` but `Arc<dyn Signer>` does not. This newtype allows an
/// `Arc`-shared signer to be passed to APIs that require `impl Signer + 'static`
/// (such as `TempoSessionMethod::with_close_signer`).
#[cfg(feature = "payments-tempo")]
struct ArcSigner(Arc<dyn mpp::Signer + Send + Sync>);

#[cfg(feature = "payments-tempo")]
#[async_trait::async_trait]
impl alloy::signers::Signer for ArcSigner {
    async fn sign_hash(
        &self,
        hash: &alloy::primitives::B256,
    ) -> alloy::signers::Result<alloy::primitives::Signature> {
        self.0.sign_hash(hash).await
    }

    fn address(&self) -> alloy::primitives::Address {
        self.0.address()
    }

    fn chain_id(&self) -> Option<alloy::primitives::ChainId> {
        self.0.chain_id()
    }

    fn set_chain_id(&mut self, _chain_id: Option<alloy::primitives::ChainId>) {
        // Arc-shared signers cannot mutate chain_id. Transaction chain_id is
        // set independently during Tempo transaction construction.
    }
}

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
    #[cfg(feature = "payments-tempo")]
    Tempo {
        mpp: TempoMpp,
        store: Arc<dyn TempoChannelStore>,
        /// RPC provider for server-initiated close transactions.
        /// Only created when `close_signer` is configured.
        provider: Option<Arc<TempoProvider>>,
        /// Signer for server-initiated close transactions.
        ///
        /// `mpp-br` accepts any alloy-compatible signer implementation here,
        /// including local keys, KMS-backed signers, and hardware wallets.
        close_signer: Option<Arc<dyn mpp::Signer + Send + Sync>>,
        escrow_contract: Address,
        chain_id: u64,
        /// TIP-20 token used to pay gas fees for close transactions.
        currency: Address,
        /// Suggested deposit for session channel opening (base units).
        suggested_deposit: Option<String>,
        /// Serializes close transactions to prevent nonce collisions when
        /// multiple channels close concurrently on the same signer.
        close_lock: tokio::sync::Mutex<()>,
    },
    #[cfg(feature = "payments-solana")]
    Solana(SolanaState),
}

impl MppBackend {
    /// The protocol-level payment method name (e.g. `"tempo"`, `"solana"`).
    ///
    /// This is the value that appears in a 402 challenge's `method` field and
    /// may differ from the CAIP-2 key used to register the backend.
    fn method_name(&self) -> &'static str {
        match self {
            #[cfg(feature = "payments-tempo")]
            Self::Tempo { .. } => "tempo",
            #[cfg(feature = "payments-solana")]
            Self::Solana(_) => "solana",
        }
    }
}

#[cfg(feature = "payments-solana")]
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
    ///
    /// `close_signer` is an optional trait-object signer for server-initiated
    /// channel close transactions. Callers provide any `alloy::signers::Signer`
    /// implementation (local key, KMS, hardware wallet, etc.) wrapped in `Arc`.
    #[cfg(feature = "payments-tempo")]
    pub fn add_tempo(
        &mut self,
        tempo: &TempoMppConfig,
        secret_key: Option<&str>,
        close_signer: Option<Arc<dyn mpp::Signer + Send + Sync>>,
    ) -> Result<(), mpp::MppError> {
        let store: Arc<dyn TempoChannelStore> = Arc::new(mpp::server::SessionChannelStore::new());
        self.add_tempo_with_store(tempo, secret_key, store, close_signer)
    }

    /// Add a Tempo backend with a caller-provided channel store.
    ///
    /// This allows injecting a custom (e.g. database-backed) store instead of
    /// the default in-memory store.
    ///
    /// `close_signer` is an optional trait-object signer for server-initiated
    /// channel close transactions. Pass any `alloy::signers::Signer`
    /// implementation wrapped in `Arc`.
    #[cfg(feature = "payments-tempo")]
    pub fn add_tempo_with_store(
        &mut self,
        tempo: &TempoMppConfig,
        secret_key: Option<&str>,
        store: Arc<dyn TempoChannelStore>,
        close_signer: Option<Arc<dyn mpp::Signer + Send + Sync>>,
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

        let escrow: Address = tempo
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

        let session_method = if let Some(ref signer) = close_signer {
            session_method.with_close_signer(ArcSigner(Arc::clone(signer)))
        } else {
            session_method
        };

        // Create a separate provider for server-initiated close operations.
        let close_provider = if close_signer.is_some() {
            Some(Arc::new(mpp::server::tempo_provider(rpc_url)?))
        } else {
            None
        };

        let mpp_instance = mpp_instance.with_session_method(session_method);

        // Resolve the TIP-20 currency address for gas fee payment on close transactions.
        let currency: Address = tempo
            .currency
            .as_deref()
            .unwrap_or(if chain_id == 42431 {
                mpp::protocol::methods::tempo::DEFAULT_CURRENCY_TESTNET
            } else {
                mpp::protocol::methods::tempo::DEFAULT_CURRENCY_MAINNET
            })
            .parse()
            .map_err(|e| mpp::MppError::InvalidConfig(format!("invalid currency: {e}")))?;

        let caip2 = format!("eip155:{chain_id}");
        self.backends.insert(
            caip2,
            MppBackend::Tempo {
                mpp: mpp_instance,
                store,
                provider: close_provider,
                close_signer,
                escrow_contract: escrow,
                chain_id,
                currency,
                suggested_deposit: tempo.default_deposit.clone(),
                close_lock: tokio::sync::Mutex::new(()),
            },
        );
        Ok(())
    }

    /// Add a Solana backend keyed by `"solana"`.
    ///
    /// Uses the default in-memory channel store.
    #[cfg(feature = "payments-solana")]
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
    #[cfg(feature = "payments-solana")]
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
            #[cfg(feature = "payments-tempo")]
            MppBackend::Tempo { store, .. } => {
                mpp::protocol::methods::tempo::session_method::deduct_from_channel(
                    &**store, channel_id, amount,
                )
                .await?;
            }
            #[cfg(feature = "payments-solana")]
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
            #[cfg(feature = "payments-tempo")]
            MppBackend::Tempo { store, .. } => {
                store.wait_for_update(channel_id).await;
            }
            #[cfg(feature = "payments-solana")]
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
            #[cfg(feature = "payments-tempo")]
            MppBackend::Tempo { store, .. } => {
                let ch = store.get_channel(channel_id).await.ok()??;
                Some((ch.spent, ch.highest_voucher_amount, ch.deposit))
            }
            #[cfg(feature = "payments-solana")]
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

    /// Close a Tempo payment channel on-chain using the highest stored voucher.
    ///
    /// Reads the channel state from the store, constructs and broadcasts a
    /// `close(channelId, cumulativeAmount, signature)` transaction to the
    /// escrow contract, then marks the channel as finalized.
    ///
    /// This is fire-and-forget: errors are returned but callers should log
    /// them rather than propagate, since close failures do not affect the
    /// already-served response. The channel can be settled later if this fails.
    // TODO: implement server-side close for Solana sessions
    #[cfg(feature = "payments-tempo")]
    pub async fn close_channel(&self, backend_key: &str, channel_id: &str) -> Result<(), String> {
        let (_key, backend) = self
            .backend_for_chain(backend_key)
            .ok_or_else(|| format!("no backend for key: {backend_key}"))?;

        let (store, provider, signer, escrow_contract, chain_id, currency, close_lock) =
            match backend {
                MppBackend::Tempo {
                    store,
                    provider,
                    close_signer,
                    escrow_contract,
                    chain_id,
                    currency,
                    close_lock,
                    ..
                } => {
                    let signer = close_signer.as_ref().ok_or("close_signer not configured")?;
                    let provider = provider.as_ref().ok_or("close provider not available")?;
                    (
                        Arc::clone(store),
                        Arc::clone(provider),
                        Arc::clone(signer),
                        *escrow_contract,
                        *chain_id,
                        *currency,
                        close_lock,
                    )
                }
                #[cfg(feature = "payments-solana")]
                MppBackend::Solana(_) => {
                    return Err("server-side close not implemented for Solana".into());
                }
            };

        // Serialize the entire read-submit-finalize flow to prevent:
        // 1. Nonce collisions when multiple channels close on the same signer
        // 2. Double-close when the same channel is closed by concurrent guards
        let _guard = close_lock.lock().await;

        let channel = store
            .get_channel(channel_id)
            .await
            .map_err(|e| format!("failed to read channel: {e}"))?
            .ok_or_else(|| format!("channel not found: {channel_id}"))?;

        if channel.finalized {
            return Ok(());
        }

        let voucher_sig = channel
            .highest_voucher_signature
            .as_ref()
            .ok_or("no voucher signature stored — nothing to settle")?;

        if channel.highest_voucher_amount == 0 {
            return Ok(());
        }

        let tx_hash = submit_close_tx(
            &provider,
            signer.as_ref(),
            escrow_contract,
            chain_id,
            channel_id,
            channel.highest_voucher_amount,
            voucher_sig,
            currency,
        )
        .await?;

        // Mark channel finalized in store.
        let channel_id_owned = channel_id.to_string();
        let voucher_sig_clone = voucher_sig.clone();
        let highest = channel.highest_voucher_amount;
        let _ = store
            .update_channel(
                &channel_id_owned,
                Box::new(move |current| {
                    let state = match current {
                        Some(s) => s,
                        None => return Ok(None),
                    };
                    Ok(Some(
                        mpp::protocol::methods::tempo::session_method::ChannelState {
                            highest_voucher_amount: highest,
                            highest_voucher_signature: Some(voucher_sig_clone),
                            finalized: true,
                            ..state
                        },
                    ))
                }),
            )
            .await
            .map_err(|e| format!("failed to finalize channel in store: {e}"))?;

        tracing::info!(
            channel_id = %channel_id_owned,
            tx_hash = %tx_hash,
            amount = highest,
            "channel closed on-chain"
        );

        Ok(())
    }
}

impl super::payment_gate::PaymentGate for MppState {
    fn verify_payment(
        &self,
        chain: Option<String>,
        auth_header: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<super::filter::MppPaymentContext, warp::Rejection>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(super::filter::verify_payment_impl(auth_header, self, chain))
    }

    fn deduct<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
        amount: u128,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), mpp::server::VerificationError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(self.deduct(backend_key, channel_id, amount))
    }

    fn wait_for_update<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.wait_for_update(backend_key, channel_id))
    }

    fn channel_balance<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u128, u128, u128)>> + Send + 'a>>
    {
        Box::pin(self.channel_balance(backend_key, channel_id))
    }

    fn close_channel<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        #[cfg(feature = "payments-tempo")]
        {
            Box::pin(self.close_channel(backend_key, channel_id))
        }
        #[cfg(not(feature = "payments-tempo"))]
        {
            let _ = (backend_key, channel_id);
            Box::pin(std::future::ready(Ok(())))
        }
    }
}

fn backend_session_challenge(
    backend: &MppBackend,
    amount: &str,
    options: SessionChallengeOptions<'_>,
) -> Result<mpp::PaymentChallenge, mpp::MppError> {
    match backend {
        #[cfg(feature = "payments-tempo")]
        MppBackend::Tempo {
            mpp,
            suggested_deposit,
            ..
        } => {
            // Merge backend-level suggested_deposit into options when the
            // caller did not provide one.
            let options = if options.suggested_deposit.is_none() {
                SessionChallengeOptions {
                    suggested_deposit: suggested_deposit.as_deref(),
                    ..options
                }
            } else {
                options
            };
            let currency = mpp
                .currency()
                .ok_or_else(|| mpp::MppError::InvalidConfig("currency not configured".into()))?;
            let recipient = mpp
                .recipient()
                .ok_or_else(|| mpp::MppError::InvalidConfig("recipient not configured".into()))?;
            mpp.session_challenge_with_details(amount, currency, recipient, options)
        }
        #[cfg(feature = "payments-solana")]
        MppBackend::Solana(state) => solana_session_challenge(state, amount, options),
    }
}

async fn backend_verify_session(
    backend: &MppBackend,
    credential: &mpp::PaymentCredential,
) -> Result<mpp::server::SessionVerifyResult, mpp::server::VerificationError> {
    match backend {
        #[cfg(feature = "payments-tempo")]
        MppBackend::Tempo { mpp, .. } => mpp.verify_session(credential).await,
        #[cfg(feature = "payments-solana")]
        MppBackend::Solana(state) => solana_verify_session(state, credential).await,
    }
}

// ── Solana helpers ───────────────────────────────────────────────────

#[cfg(feature = "payments-solana")]
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

#[cfg(feature = "payments-solana")]
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

// ── Tempo close helper ───────────────────────────────────────────────

/// Build, sign, and broadcast a `close(channelId, cumulativeAmount, signature)`
/// transaction on the Tempo escrow contract.
///
/// Returns the transaction hash on success.
#[cfg(feature = "payments-tempo")]
#[allow(clippy::too_many_arguments)]
async fn submit_close_tx(
    provider: &TempoProvider,
    signer: &(dyn mpp::Signer + Send + Sync),
    escrow_contract: Address,
    chain_id: u64,
    channel_id: &str,
    cumulative_amount: u128,
    voucher_signature: &[u8],
    fee_token: Address,
) -> Result<String, String> {
    use alloy::eips::Encodable2718;
    use alloy::primitives::{B256, Bytes};
    use alloy::providers::Provider;
    use alloy::sol_types::SolCall;
    use tempo_primitives::TempoTransaction;
    use tempo_primitives::transaction::Call;

    alloy::sol! {
        interface IEscrowClose {
            function close(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) external;
        }
    }

    let channel_id_b256: B256 = channel_id
        .parse()
        .map_err(|e| format!("invalid channel_id: {e}"))?;

    let close_data = IEscrowClose::closeCall::new((
        channel_id_b256,
        cumulative_amount,
        Bytes::from(voucher_signature.to_vec()),
    ))
    .abi_encode();

    let nonce = provider
        .get_transaction_count(signer.address())
        .await
        .map_err(|e| format!("failed to get nonce: {e}"))?;

    let gas_price = provider
        .get_gas_price()
        .await
        .map_err(|e| format!("failed to get gas price: {e}"))?;

    let tempo_tx = TempoTransaction {
        chain_id,
        nonce,
        gas_limit: 2_000_000,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: gas_price,
        calls: vec![Call {
            to: alloy::primitives::TxKind::Call(escrow_contract),
            value: alloy::primitives::U256::ZERO,
            input: Bytes::from(close_data),
        }],
        fee_token: Some(fee_token),
        ..Default::default()
    };

    let sig_hash = tempo_tx.signature_hash();
    let signature = signer
        .sign_hash(&sig_hash)
        .await
        .map_err(|e| format!("failed to sign close tx: {e}"))?;

    let signed_tx = tempo_tx.into_signed(signature.into());
    let tx_bytes = Bytes::from(signed_tx.encoded_2718());

    let pending = provider
        .send_raw_transaction(&tx_bytes)
        .await
        .map_err(|e| format!("failed to send close tx: {e}"))?;

    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("close tx failed: {e}"))?;

    Ok(receipt.transaction_hash.to_string())
}
