//! `bitrouter key` subcommands — manage OWS API keys for agent access.

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Password};

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
        println!("  Policies: {}", key_file.policy_ids.join(", "));
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

/// Revoke (delete) an OWS API key by ID.
pub fn revoke(id: &str) -> Result {
    let theme = ColorfulTheme::default();

    let confirmed = Confirm::with_theme(&theme)
        .with_prompt(format!("Revoke API key '{id}'? This cannot be undone"))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    ows_lib::key_store::delete_api_key(id, None)?;
    println!("API key '{id}' revoked.");

    Ok(())
}
