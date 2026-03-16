//! `bitrouter sudo` subcommand — master-wallet signing operations with Swig.
//!
//! All signing subcommands prompt for the master wallet passphrase before
//! delegating to Swig placeholder functions. The `show-wallet` subcommand
//! displays wallet info without requiring a signature.

use std::path::Path;

use dialoguer::theme::ColorfulTheme;

use crate::cli::onboarding::{self, AgentWalletState};
use crate::cli::swig;

/// Run `bitrouter sudo create-embedded-wallet`.
pub fn run_create_embedded_wallet(home_dir: &Path) -> Result<(), String> {
    let theme = ColorfulTheme::default();
    let state = onboarding::load_state(home_dir);

    let wallet_path = state
        .master_wallet_path
        .as_deref()
        .ok_or("no master wallet configured — run onboarding first or import a wallet")?;

    let passphrase = onboarding::prompt_passphrase(&theme)
        .map_err(|e| format!("passphrase prompt failed: {e}"))?;

    println!("  Creating Swig embedded wallet...");
    match swig::create_embedded_wallet(wallet_path, passphrase.as_bytes()) {
        Ok(info) => {
            println!("  ✓ Embedded wallet created: {}", info.address);

            let mut state = onboarding::load_state(home_dir);
            state.embedded_wallet_address = Some(info.address);
            onboarding::save_state(home_dir, &state)?;

            Ok(())
        }
        Err(e) => Err(format!("failed to create embedded wallet: {e}")),
    }
}

/// Run `bitrouter sudo derive-agent-wallet`.
pub fn run_derive_agent_wallet(
    home_dir: &Path,
    per_tx_cap: Option<u64>,
    cumulative_cap: Option<u64>,
    expiration: Option<String>,
) -> Result<(), String> {
    let theme = ColorfulTheme::default();
    let state = onboarding::load_state(home_dir);

    let wallet_path = state
        .master_wallet_path
        .as_deref()
        .ok_or("no master wallet configured — run onboarding first or import a wallet")?;

    let passphrase = onboarding::prompt_passphrase(&theme)
        .map_err(|e| format!("passphrase prompt failed: {e}"))?;

    let expires_at = match expiration.as_deref() {
        Some(s) => parse_expiration_flag(s)?,
        None => None,
    };

    let permissions = swig::AgentPermissions {
        per_tx_cap,
        cumulative_cap,
        expires_at,
    };

    println!("  Deriving agent wallet with Swig...");
    match swig::derive_agent_wallet(wallet_path, passphrase.as_bytes(), &permissions) {
        Ok(info) => {
            println!("  ✓ Agent wallet derived: {}", info.address);
            print_permissions(&info.permissions);

            let agent = AgentWalletState {
                address: info.address,
                permissions: info.permissions,
            };
            onboarding::save_agent_wallet(home_dir, &agent)?;

            Ok(())
        }
        Err(e) => Err(format!("failed to derive agent wallet: {e}")),
    }
}

/// Run `bitrouter sudo set-permissions`.
pub fn run_set_permissions(
    home_dir: &Path,
    agent_address: Option<String>,
    per_tx_cap: Option<u64>,
    cumulative_cap: Option<u64>,
    expiration: Option<String>,
) -> Result<(), String> {
    let theme = ColorfulTheme::default();
    let state = onboarding::load_state(home_dir);

    let wallet_path = state
        .master_wallet_path
        .as_deref()
        .ok_or("no master wallet configured — run onboarding first or import a wallet")?;

    // Resolve agent address: explicit flag > persisted state
    let agent_addr = agent_address
        .or_else(|| onboarding::load_agent_wallet(home_dir).map(|a| a.address))
        .ok_or("no agent wallet address — specify with --agent or derive one first")?;

    let passphrase = onboarding::prompt_passphrase(&theme)
        .map_err(|e| format!("passphrase prompt failed: {e}"))?;

    let expires_at = match expiration.as_deref() {
        Some(s) => parse_expiration_flag(s)?,
        None => None,
    };

    let permissions = swig::AgentPermissions {
        per_tx_cap,
        cumulative_cap,
        expires_at,
    };

    println!("  Updating agent wallet permissions via Swig...");
    match swig::set_agent_permissions(
        wallet_path,
        passphrase.as_bytes(),
        &agent_addr,
        &permissions,
    ) {
        Ok(updated) => {
            println!("  ✓ Permissions updated for {agent_addr}");
            print_permissions(&updated);

            // Update local reference
            let agent = AgentWalletState {
                address: agent_addr,
                permissions: updated,
            };
            onboarding::save_agent_wallet(home_dir, &agent)?;

            Ok(())
        }
        Err(e) => Err(format!("failed to set permissions: {e}")),
    }
}

/// Run `bitrouter sudo show-wallet` (no signing required).
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

    if let Some(ref addr) = state.embedded_wallet_address {
        println!("  Embedded wallet: {addr}");
    } else {
        println!("  Embedded wallet: not created");
    }

    match onboarding::load_agent_wallet(home_dir) {
        Some(agent) => {
            println!("  Agent wallet: {}", agent.address);
            print_permissions(&agent.permissions);
        }
        None => {
            println!("  Agent wallet: not derived");
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

fn print_permissions(p: &swig::AgentPermissions) {
    if let Some(cap) = p.per_tx_cap {
        println!("    per-tx cap:     {cap}");
    }
    if let Some(cap) = p.cumulative_cap {
        println!("    cumulative cap: {cap}");
    }
    match p.expires_at {
        Some(ts) => println!("    expires at:     {ts} (unix)"),
        None => println!("    expires at:     never"),
    }
}

fn parse_expiration_flag(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() || s == "never" {
        return Ok(None);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_secs();

    if let Some(days) = s.strip_suffix('d') {
        let d: u64 = days
            .parse()
            .map_err(|_| format!("invalid duration \"{s}\""))?;
        return Ok(Some(now + d * 86400));
    }
    if let Some(hours) = s.strip_suffix('h') {
        let h: u64 = hours
            .parse()
            .map_err(|_| format!("invalid duration \"{s}\""))?;
        return Ok(Some(now + h * 3600));
    }

    if let Ok(ts) = s.parse::<u64>() {
        return Ok(Some(ts));
    }

    Err(format!(
        "invalid expiration \"{s}\" — use \"7d\", \"30d\", \"never\", or a UNIX timestamp"
    ))
}
