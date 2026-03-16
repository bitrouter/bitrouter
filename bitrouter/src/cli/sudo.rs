//! `bitrouter sudo` subcommand — master-wallet signing operations with Swig.
//!
//! All signing subcommands prompt for the master wallet passphrase before
//! delegating to Swig SDK functions. The `show-wallet` subcommand displays
//! wallet info and live on-chain balances without requiring a signature.

use std::path::Path;

use dialoguer::theme::ColorfulTheme;

use crate::cli::onboarding::{self, AgentWalletState};
use crate::cli::swig;

/// Resolve the Solana RPC URL: explicit flag > onboarding state > default.
fn resolve_rpc_url(home_dir: &Path, explicit: Option<&str>) -> String {
    if let Some(url) = explicit {
        return url.to_string();
    }
    let state = onboarding::load_state(home_dir);
    state
        .rpc_url
        .unwrap_or_else(|| swig::DEFAULT_RPC_URL.to_string())
}

/// Resolve the Swig account address from onboarding state.
fn resolve_swig_account(home_dir: &Path) -> Result<String, String> {
    onboarding::load_state(home_dir)
        .embedded_wallet_address
        .ok_or_else(|| {
            "no embedded wallet — run onboarding or `bitrouter sudo create-embedded-wallet` first"
                .to_string()
        })
}

/// Run `bitrouter sudo create-embedded-wallet`.
pub fn run_create_embedded_wallet(home_dir: &Path) -> Result<(), String> {
    let theme = ColorfulTheme::default();
    let state = onboarding::load_state(home_dir);

    let wallet_path = state
        .master_wallet_path
        .as_deref()
        .ok_or("no master wallet configured — run onboarding first or import a wallet")?;

    let rpc_url = resolve_rpc_url(home_dir, None);

    let passphrase = onboarding::prompt_passphrase(&theme)
        .map_err(|e| format!("passphrase prompt failed: {e}"))?;

    println!("  Creating Swig embedded wallet...");
    match swig::create_embedded_wallet(wallet_path, passphrase.as_bytes(), &rpc_url) {
        Ok(info) => {
            println!("  ✓ Embedded wallet created: {}", info.address);
            println!("  Wallet address (for funding): {}", info.wallet_address);

            let mut state = onboarding::load_state(home_dir);
            state.embedded_wallet_address = Some(info.address);
            state.wallet_address = Some(info.wallet_address);
            state.swig_id = Some(info.swig_id);
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
    label: Option<String>,
) -> Result<(), String> {
    let theme = ColorfulTheme::default();
    let state = onboarding::load_state(home_dir);

    let wallet_path = state
        .master_wallet_path
        .as_deref()
        .ok_or("no master wallet configured — run onboarding first or import a wallet")?;

    let rpc_url = resolve_rpc_url(home_dir, None);
    let swig_account = resolve_swig_account(home_dir)?;

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

    let label = label.unwrap_or_else(|| "default".to_string());

    println!("  Deriving agent wallet with Swig...");
    match swig::derive_agent_wallet(
        wallet_path,
        passphrase.as_bytes(),
        &permissions,
        &rpc_url,
        &label,
        home_dir,
        &swig_account,
    ) {
        Ok((info, _keypair_bytes)) => {
            println!(
                "  ✓ Agent wallet derived: {} ({})",
                info.address, info.label
            );
            print_permissions(&info.permissions);

            let agent = AgentWalletState {
                label: info.label,
                address: info.address,
                role_id: info.role_id,
                permissions: info.permissions,
                created_at: info.created_at,
            };
            onboarding::add_agent_wallet(home_dir, agent)?;

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

    let rpc_url = resolve_rpc_url(home_dir, None);
    let swig_account = resolve_swig_account(home_dir)?;

    // Resolve agent: explicit flag > first persisted agent
    let (agent_addr, agent_role_id) = if let Some(ref addr) = agent_address {
        // Look up role_id from persisted state
        let role_id = onboarding::load_agent_wallets(home_dir)
            .iter()
            .find(|a| a.address == *addr)
            .map(|a| a.role_id)
            .ok_or_else(|| {
                format!("agent {addr} not found in persisted state — specify role_id")
            })?;
        (addr.clone(), role_id)
    } else {
        let agent = onboarding::load_agent_wallet(home_dir)
            .ok_or("no agent wallet — specify with --agent or derive one first")?;
        (agent.address, agent.role_id)
    };

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
        &rpc_url,
        &swig_account,
        agent_role_id,
    ) {
        Ok(updated) => {
            println!("  ✓ Permissions updated for {agent_addr}");
            print_permissions(&updated);

            // Update local reference — find by address and update permissions
            let agents = onboarding::load_agent_wallets(home_dir);
            if let Some(existing) = agents.iter().find(|a| a.address == agent_addr) {
                let agent = AgentWalletState {
                    label: existing.label.clone(),
                    address: agent_addr,
                    role_id: existing.role_id,
                    permissions: updated,
                    created_at: existing.created_at.clone(),
                };
                onboarding::add_agent_wallet(home_dir, agent)?;
            }

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

    let rpc_url = state.rpc_url.as_deref().unwrap_or(swig::DEFAULT_RPC_URL);
    println!("  RPC: {rpc_url}");

    if let Some(ref addr) = state.embedded_wallet_address {
        println!("  Embedded wallet: {addr}");
    } else {
        println!("  Embedded wallet: not created");
    }

    if let Some(ref addr) = state.wallet_address {
        println!("  Wallet address: {addr}");

        // Fetch live balance.
        match swig::get_balance(rpc_url, addr) {
            Ok(bal) => {
                println!(
                    "  Balance: {}  |  {}",
                    bal.sol_display(),
                    bal.usdc_display()
                );
            }
            Err(e) => {
                println!("  Balance: unavailable ({e})");
            }
        }
    }

    let agents = onboarding::load_agent_wallets(home_dir);
    if agents.is_empty() {
        println!("  Agent wallets: none");
    } else {
        println!("  Agent wallets:");
        for agent in &agents {
            println!(
                "    [{label}] {addr}  (role {role}, created {ts})",
                label = agent.label,
                addr = agent.address,
                role = agent.role_id,
                ts = agent.created_at,
            );
            print_permissions(&agent.permissions);
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
