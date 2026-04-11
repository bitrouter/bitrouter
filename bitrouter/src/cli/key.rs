//! `bitrouter key` subcommands — manage OWS API keys for agent access.

use dialoguer::Password;
use dialoguer::theme::ColorfulTheme;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Create a new OWS API key scoped to specific wallets (and optionally policies).
pub fn create(
    name: &str,
    wallets: &[String],
    policies: &[String],
    expires_at: Option<&str>,
) -> Result {
    let theme = ColorfulTheme::default();

    let passphrase = Password::with_theme(&theme)
        .with_prompt("Wallet owner passphrase")
        .allow_empty_password(true)
        .interact()?;

    let (token, key_file) =
        ows_lib::key_ops::create_api_key(name, wallets, policies, &passphrase, expires_at, None)?;

    // Metadata only — name, ID, wallet IDs, and policy IDs are not secrets.
    println!("API key created: {}", key_file.name);
    println!("  ID:       {}", key_file.id);
    println!("  Wallets:  {}", key_file.wallet_ids.join(", "));
    if !key_file.policy_ids.is_empty() {
        println!("  Policy:   {}", key_file.policy_ids.join(", "));
    }
    if let Some(ref exp) = key_file.expires_at {
        println!("  Expires:  {exp}");
    }
    println!();
    // Intentional: OWS API tokens are displayed exactly once at creation.
    // The operator copies the token to provision it to the agent; OWS only
    // stores the SHA-256 hash, so the raw token cannot be recovered later.
    println!("  Token (shown once — save it now):");
    println!("  {token}");

    Ok(())
}

/// List all OWS API keys.
pub fn list() -> Result {
    let keys = ows_lib::key_store::list_api_keys(None)?;

    if keys.is_empty() {
        println!("No API keys found. Run `bitrouter key create` to create one.");
        return Ok(());
    }

    // Metadata only — names, IDs, expiry, and wallet IDs are not secrets.
    // Raw tokens are never stored; list_api_keys returns only SHA-256 hashes.
    println!("{:<20} {:<38} {:<12} Wallets", "NAME", "ID", "EXPIRES");
    println!("{}", "-".repeat(80));
    for k in &keys {
        let expires = k.expires_at.as_deref().unwrap_or("never");
        println!(
            "{:<20} {:<38} {:<12} {}",
            k.name,
            k.id,
            expires,
            k.wallet_ids.join(", "),
        );
    }

    Ok(())
}

/// Revoke an API key `id` on the running server by notifying its admin endpoint.
///
/// Sends `POST /admin/keys/revoke` with the key ID to add it to the
/// server's deny-list. All JWTs bearing this `id` claim are immediately
/// rejected regardless of their `exp`.
pub fn revoke_on_server(
    config: &bitrouter_config::BitrouterConfig,
    addr: std::net::SocketAddr,
    id: &str,
) -> Result {
    let url = format!("http://{addr}/admin/keys/revoke");
    let client = reqwest::blocking::Client::new();
    let resp = crate::cli::admin_auth::request_with_admin_auth(config, client.post(&url))?
        .json(&serde_json::json!({ "id": id }))
        .send()?;

    if resp.status().is_success() {
        println!("API key '{id}' revoked on server.");
    } else {
        let msg = crate::cli::admin_auth::parse_error_message(resp)?;
        return Err(format!("failed to revoke key: {msg}").into());
    }

    Ok(())
}

/// Sign a JWT for agent access — the operator mints tokens that agents
/// present as bearer auth to the running BitRouter server.
pub fn sign(
    wallet_name: &str,
    models: Option<&[String]>,
    budget: Option<u64>,
    budget_scope: Option<&str>,
    exp: Option<&str>,
    ows_key: Option<&str>,
    policy: Option<&str>,
) -> Result {
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use bitrouter_core::auth::chain::{Caip10, Chain};
    use bitrouter_core::auth::claims::{BitrouterClaims, BudgetScope, TokenScope};
    use bitrouter_core::auth::token;
    use dialoguer::Password;
    use sha2::{Digest, Sha256};

    // 1. Load wallet and resolve Solana address for CAIP-10 iss.
    let info = ows_lib::get_wallet(wallet_name, None)
        .map_err(|e| format!("failed to load wallet '{wallet_name}': {e}"))?;

    let sol_account = info
        .accounts
        .iter()
        .find(|a| a.chain_id.starts_with("solana:"))
        .ok_or_else(|| format!("wallet '{wallet_name}' has no Solana account — cannot sign JWT"))?;

    let caip10 = Caip10 {
        chain: Chain::solana_mainnet(),
        address: sol_account.address.clone(),
    };

    // 2. Parse optional budget scope.
    let bsc = match budget_scope {
        Some("session" | "ses") => Some(BudgetScope::Session),
        Some("account" | "act") => Some(BudgetScope::Account),
        Some(other) => {
            return Err(
                format!("invalid budget scope '{other}': use 'session' or 'account'").into(),
            );
        }
        None => None,
    };

    // 3. Parse expiration duration and compute absolute timestamp.
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let exp_ts = match exp {
        Some(raw) => Some(now + parse_duration_secs(raw)?),
        None => None,
    };

    // 4. Generate API key identity (`id` claim).
    //    - OWS-backed keys: deterministically derived via SHA-256 of the key string.
    //    - Standalone keys: randomly generated 32-byte value.
    let key_id = match ows_key {
        Some(key_str) => {
            let hash = Sha256::digest(key_str.as_bytes());
            URL_SAFE_NO_PAD.encode(hash)
        }
        None => {
            let bytes: [u8; 32] = rand::random();
            URL_SAFE_NO_PAD.encode(bytes)
        }
    };

    // 5. Construct claims.
    let claims = BitrouterClaims {
        iss: caip10.format(),
        iat: Some(now),
        exp: exp_ts,
        scp: Some(TokenScope::Api),
        mdl: models.map(|m| m.to_vec()),
        bgt: budget,
        bsc,
        id: Some(key_id),
        key: ows_key.map(String::from),
        pol: policy.map(String::from),
    };

    // 6. Prompt passphrase and sign.
    let theme = ColorfulTheme::default();
    let passphrase = Password::with_theme(&theme)
        .with_prompt("Wallet owner passphrase")
        .allow_empty_password(true)
        .interact()?;

    let signer = OwsJwtSigner {
        wallet_name: wallet_name.to_owned(),
        passphrase,
        vault_path: None,
    };

    let jwt = token::sign(&claims, &signer).map_err(|e| format!("failed to sign JWT: {e}"))?;

    println!("{jwt}");

    Ok(())
}

/// Parse a human-readable duration string into seconds.
///
/// Accepts raw seconds (e.g. `"3600"`) or suffixed durations:
/// `"30s"`, `"12h"`, `"7d"`, `"1m"` (m = minutes).
fn parse_duration_secs(input: &str) -> Result<u64> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty duration".into());
    }

    let last = input.as_bytes()[input.len() - 1];
    if last.is_ascii_digit() {
        return input
            .parse::<u64>()
            .map_err(|_| format!("invalid duration '{input}'").into());
    }

    let (num_str, multiplier) = match last {
        b's' => (&input[..input.len() - 1], 1u64),
        b'm' => (&input[..input.len() - 1], 60),
        b'h' => (&input[..input.len() - 1], 3_600),
        b'd' => (&input[..input.len() - 1], 86_400),
        _ => return Err(format!("unknown duration suffix '{}'", last as char).into()),
    };

    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration number '{num_str}'"))?;

    Ok(n * multiplier)
}

/// OWS-backed JWT signer for the `key sign` command.
///
/// Decrypts the wallet key on each signing call. Key material is zeroized
/// on drop by the OWS SDK.
struct OwsJwtSigner {
    wallet_name: String,
    passphrase: String,
    vault_path: Option<String>,
}

impl bitrouter_core::auth::keys::JwtSigner for OwsJwtSigner {
    fn sign_ed25519(
        &self,
        message: &[u8],
    ) -> std::result::Result<Vec<u8>, bitrouter_core::auth::JwtError> {
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

    fn sign_eip191(
        &self,
        message: &[u8],
    ) -> std::result::Result<Vec<u8>, bitrouter_core::auth::JwtError> {
        let vault = self.vault_path.as_deref().map(std::path::Path::new);
        let key = ows_lib::decrypt_signing_key(
            &self.wallet_name,
            ows_core::ChainType::Evm,
            &self.passphrase,
            None,
            vault,
        )
        .map_err(|e| bitrouter_core::auth::JwtError::Signing(e.to_string()))?;

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
