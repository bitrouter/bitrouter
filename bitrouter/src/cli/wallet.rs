//! `bitrouter wallet` subcommands — thin wrappers over the OWS SDK.

use std::path::Path;

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Password};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Create a new OWS wallet with a fresh BIP-39 mnemonic.
pub fn create(name: &str, words: Option<u32>, show_mnemonic: bool) -> Result {
    let theme = ColorfulTheme::default();

    let passphrase = Password::with_theme(&theme)
        .with_prompt("Set passphrase (leave empty for none)")
        .allow_empty_password(true)
        .with_confirmation("Confirm passphrase", "Passphrases do not match")
        .interact()?;

    let pass = if passphrase.is_empty() {
        None
    } else {
        Some(passphrase.as_str())
    };

    let info = ows_lib::create_wallet(name, words, pass, None)?;

    println!("Wallet created: {}", info.name);
    println!("  ID: {}", info.id);
    print_accounts(&info.accounts);

    if show_mnemonic {
        let mnemonic = ows_lib::export_wallet(&info.name, pass, None)?;
        println!("\n  Mnemonic (back this up!):\n  {mnemonic}");
    }

    Ok(())
}

/// Import a wallet from a BIP-39 mnemonic phrase.
pub fn import_mnemonic(name: &str, index: Option<u32>) -> Result {
    let theme = ColorfulTheme::default();

    let mnemonic = Password::with_theme(&theme)
        .with_prompt("Mnemonic phrase")
        .allow_empty_password(false)
        .interact()?;

    let passphrase = Password::with_theme(&theme)
        .with_prompt("Set passphrase (leave empty for none)")
        .allow_empty_password(true)
        .with_confirmation("Confirm passphrase", "Passphrases do not match")
        .interact()?;

    let pass = if passphrase.is_empty() {
        None
    } else {
        Some(passphrase.as_str())
    };

    let info = ows_lib::import_wallet_mnemonic(name, &mnemonic, pass, index, None)?;

    println!("Wallet imported: {}", info.name);
    println!("  ID: {}", info.id);
    print_accounts(&info.accounts);

    Ok(())
}

/// Import a wallet from a raw hex private key (e.g. legacy MasterKeypair seed).
pub fn import_private_key(name: &str, chain: Option<&str>) -> Result {
    let theme = ColorfulTheme::default();

    let key_hex = Password::with_theme(&theme)
        .with_prompt("Private key (hex)")
        .allow_empty_password(false)
        .interact()?;

    let passphrase = Password::with_theme(&theme)
        .with_prompt("Set passphrase (leave empty for none)")
        .allow_empty_password(true)
        .with_confirmation("Confirm passphrase", "Passphrases do not match")
        .interact()?;

    let pass = if passphrase.is_empty() {
        None
    } else {
        Some(passphrase.as_str())
    };

    let info = ows_lib::import_wallet_private_key(name, &key_hex, chain, pass, None, None, None)?;

    println!("Wallet imported: {}", info.name);
    println!("  ID: {}", info.id);
    print_accounts(&info.accounts);

    Ok(())
}

/// List all wallets in the OWS vault.
pub fn list(vault_path: Option<&Path>) -> Result {
    let wallets = ows_lib::list_wallets(vault_path)?;

    if wallets.is_empty() {
        println!("No wallets found. Run `bitrouter wallet create` to get started.");
        return Ok(());
    }

    println!("{:<20} {:<38} Chains", "NAME", "ID");
    println!("{}", "-".repeat(72));
    for w in &wallets {
        let chains: Vec<&str> = w
            .accounts
            .iter()
            .map(|a| chain_label(&a.chain_id))
            .collect();
        println!("{:<20} {:<38} {}", w.name, w.id, chains.join(", "));
    }

    Ok(())
}

/// Show detailed info for a single wallet.
pub fn info(name: &str, vault_path: Option<&Path>) -> Result {
    let w = ows_lib::get_wallet(name, vault_path)?;

    println!("Name:       {}", w.name);
    println!("ID:         {}", w.id);
    println!("Created:    {}", w.created_at);
    println!("Accounts:");
    for a in &w.accounts {
        println!("  {} ({})", a.address, chain_label(&a.chain_id));
        println!("    path: {}", a.derivation_path);
    }

    Ok(())
}

/// Export a wallet's mnemonic phrase.
pub fn export(name: &str) -> Result {
    let theme = ColorfulTheme::default();

    let passphrase = Password::with_theme(&theme)
        .with_prompt("Wallet passphrase")
        .allow_empty_password(true)
        .interact()?;

    let pass = if passphrase.is_empty() {
        None
    } else {
        Some(passphrase.as_str())
    };

    let mnemonic = ows_lib::export_wallet(name, pass, None)?;
    println!("{mnemonic}");

    Ok(())
}

/// Delete a wallet from the OWS vault.
pub fn delete(name: &str) -> Result {
    let theme = ColorfulTheme::default();

    let confirmed = Confirm::with_theme(&theme)
        .with_prompt(format!("Delete wallet '{name}'? This cannot be undone"))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    ows_lib::delete_wallet(name, None)?;
    println!("Wallet '{name}' deleted.");

    Ok(())
}

/// Rename a wallet.
pub fn rename(name: &str, new_name: &str) -> Result {
    ows_lib::rename_wallet(name, new_name, None)?;
    println!("Wallet renamed: {name} → {new_name}");

    Ok(())
}

fn print_accounts(accounts: &[ows_lib::AccountInfo]) {
    if accounts.is_empty() {
        return;
    }
    println!("  Accounts:");
    for a in accounts {
        println!("    {} ({})", a.address, chain_label(&a.chain_id));
    }
}

fn chain_label(chain_id: &str) -> &str {
    if chain_id.starts_with("eip155:") {
        "evm"
    } else if chain_id.starts_with("solana:") {
        "solana"
    } else if chain_id.starts_with("bip122:") {
        "bitcoin"
    } else {
        chain_id
    }
}
