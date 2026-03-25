//! `bitrouter sudo` subcommand — wallet status and diagnostics.

use std::path::Path;

use crate::cli::account;
use crate::cli::onboarding;

/// Run `bitrouter sudo show-wallet` (read-only).
pub fn run_show_wallet(home_dir: &Path) -> Result<(), String> {
    let state = onboarding::load_state(home_dir);

    println!("  Wallet Status");
    println!("  ─────────────");
    println!("  Onboarding: {:?}", state.status);

    if let Some(ref prefix) = state.keypair_prefix {
        println!("  Keypair prefix: {prefix}");
    } else {
        println!("  Keypair: not configured");
    }

    let keys_dir = home_dir.join(".keys");
    match account::load_active_keypair(&keys_dir) {
        Ok((prefix, kp)) => {
            let evm_addr = kp
                .evm_address_string()
                .unwrap_or_else(|_| "unknown".to_string());
            let sol_addr = kp.solana_pubkey_b58();
            println!("  Active key: {prefix}");
            println!("  EVM:    {evm_addr}");
            println!("  Solana: {sol_addr}");
        }
        Err(_) => {
            println!("  Active key: none");
        }
    }

    Ok(())
}
