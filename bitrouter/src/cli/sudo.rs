//! `bitrouter sudo` subcommand — wallet status and diagnostics.

use std::path::Path;

use crate::cli::onboarding;
use crate::cli::swig;

/// Run `bitrouter sudo show-wallet` (read-only).
pub fn run_show_wallet(home_dir: &Path) -> Result<(), String> {
    let state = onboarding::load_state(home_dir);

    println!("  Wallet Status");
    println!("  ─────────────");
    println!("  Onboarding: {:?}", state.status);

    if let Some(ref path) = state.master_wallet_path {
        println!("  Master wallet: {}", path.display());
    } else {
        println!("  Master wallet: not configured");
    }

    let rpc_url = state.rpc_url.as_deref().unwrap_or(swig::DEFAULT_RPC_URL);
    println!("  RPC: {rpc_url}");

    Ok(())
}
