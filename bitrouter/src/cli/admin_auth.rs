//! Admin JWT generation and HTTP auth helpers for CLI commands.
//!
//! Produces short-lived admin JWTs for authenticating CLI requests to the
//! running daemon (e.g. `route list`, `tools list`, `agents list`).
//!
//! When a wallet is configured, the JWT is signed using the OWS wallet.
//! Otherwise, requests are sent without authentication (the server may
//! reject them if auth is required).

use std::net::SocketAddr;

use reqwest::blocking::{RequestBuilder, Response};

/// Generate a short-lived admin JWT for local daemon management.
///
/// When a wallet is configured, the JWT is signed with the wallet's
/// Solana key. Returns `None` when no wallet is available.
pub fn generate_local_admin_jwt(
    config: &bitrouter_config::BitrouterConfig,
) -> Result<Option<String>, String> {
    if let Some(wallet) = config.wallet.as_ref() {
        let jwt = ows_sign_admin_jwt(wallet)?;
        return Ok(Some(jwt));
    }

    Ok(None)
}

/// Attach an admin JWT (if available) to an outgoing HTTP request.
pub fn request_with_admin_auth(
    config: &bitrouter_config::BitrouterConfig,
    request: RequestBuilder,
) -> Result<RequestBuilder, Box<dyn std::error::Error>> {
    match generate_local_admin_jwt(config) {
        Ok(Some(jwt)) => Ok(request.bearer_auth(jwt)),
        Ok(None) => Ok(request),
        Err(e) => Err(e.into()),
    }
}

/// Build a daemon URL and wrap a GET request with admin auth.
pub fn admin_get(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
    path: &str,
) -> Result<Response, Box<dyn std::error::Error>> {
    let url = format!("http://{addr}{path}");
    let client = reqwest::blocking::Client::new();
    let resp = request_with_admin_auth(config, client.get(&url))?.send()?;
    Ok(resp)
}

/// Extract a human-readable error message from a failed HTTP response.
pub fn parse_error_message(response: Response) -> Result<String, Box<dyn std::error::Error>> {
    let status = response.status();
    let body = response.text()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();

    if let Some(message) = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|value| value.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        return Ok(message.to_owned());
    }

    if body.trim().is_empty() {
        Ok(format!("request failed with status {status}"))
    } else {
        Ok(format!("request failed with status {status}: {body}"))
    }
}

// ── OWS wallet JWT signing ───────────────────────────────────

fn ows_sign_admin_jwt(wallet: &bitrouter_config::config::WalletConfig) -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    use bitrouter_core::auth::chain::{Caip10, Chain};
    use bitrouter_core::auth::claims::{BitrouterClaims, TokenScope};
    use bitrouter_core::auth::token;

    let vault = wallet.vault_path.as_deref().map(std::path::Path::new);
    let info = ows_lib::get_wallet(&wallet.name, vault)
        .map_err(|e| format!("failed to load wallet '{}': {e}", wallet.name))?;

    let sol_account = info
        .accounts
        .iter()
        .find(|a| a.chain_id.starts_with("solana:"))
        .ok_or_else(|| {
            format!(
                "wallet '{}' has no Solana account — cannot sign admin JWT",
                wallet.name
            )
        })?;

    let chain = Chain::solana_mainnet();
    let caip10 = Caip10 {
        chain: chain.clone(),
        address: sol_account.address.clone(),
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_secs();

    let claims = BitrouterClaims {
        iss: caip10.format(),
        chain: chain.caip2(),
        iat: Some(now),
        exp: Some(now + 300), // 5 minutes
        scope: TokenScope::Admin,
        models: None,
        tools: None,
        budget: None,
        budget_scope: None,
        budget_range: None,
    };

    let signer = OwsJwtSigner::new(wallet)?;
    token::sign(&claims, &signer).map_err(|e| format!("failed to sign admin JWT: {e}"))
}

/// OWS-backed [`JwtSigner`] for admin JWT generation.
///
/// Decrypts the wallet key on each signing call. Key material is zeroized
/// on drop by the OWS SDK.
struct OwsJwtSigner {
    wallet_name: String,
    passphrase: String,
    vault_path: Option<String>,
}

impl OwsJwtSigner {
    fn new(wallet: &bitrouter_config::config::WalletConfig) -> Result<Self, String> {
        let passphrase = std::env::var("OWS_PASSPHRASE").unwrap_or_default();
        Ok(Self {
            wallet_name: wallet.name.clone(),
            passphrase,
            vault_path: wallet.vault_path.clone(),
        })
    }
}

impl bitrouter_core::auth::keys::JwtSigner for OwsJwtSigner {
    fn sign_ed25519(&self, message: &[u8]) -> Result<Vec<u8>, bitrouter_core::auth::JwtError> {
        let vault = self.vault_path.as_deref().map(std::path::Path::new);
        let key = ows_lib::decrypt_signing_key(
            &self.wallet_name,
            ows_core::ChainType::Solana,
            &self.passphrase,
            None,
            vault,
        )
        .map_err(|e| bitrouter_core::auth::JwtError::Signing(e.to_string()))?;

        let signer = ows_signer::signer_for_chain(ows_core::ChainType::Solana);
        let output = signer
            .sign(key.expose(), message)
            .map_err(|e| bitrouter_core::auth::JwtError::Signing(e.to_string()))?;

        Ok(output.signature)
    }

    fn sign_eip191(&self, message: &[u8]) -> Result<Vec<u8>, bitrouter_core::auth::JwtError> {
        let vault = self.vault_path.as_deref().map(std::path::Path::new);
        let key = ows_lib::decrypt_signing_key(
            &self.wallet_name,
            ows_core::ChainType::Evm,
            &self.passphrase,
            None,
            vault,
        )
        .map_err(|e| bitrouter_core::auth::JwtError::Signing(e.to_string()))?;

        // Apply EIP-191 prefix and keccak256 before signing the raw hash.
        let prefix = format!("\x19Ethereum Signed Message:\n{}", message.len());
        let mut data = Vec::with_capacity(prefix.len() + message.len());
        data.extend_from_slice(prefix.as_bytes());
        data.extend_from_slice(message);
        let hash = alloy::primitives::keccak256(&data);

        let signer = ows_signer::signer_for_chain(ows_core::ChainType::Evm);
        let output = signer
            .sign(key.expose(), hash.as_ref())
            .map_err(|e| bitrouter_core::auth::JwtError::Signing(e.to_string()))?;

        Ok(output.signature)
    }
}
