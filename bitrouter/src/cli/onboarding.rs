//! First-run onboarding flow for BitRouter Node.
//!
//! Auto-triggered on first `serve` / `start` when no onboarding state marker
//! exists. Guides the user through web3 wallet setup and node provider config.

use std::fs;
use std::path::Path;

use dialoguer::{Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

use crate::cli::account;

// ── Onboarding state model ────────────────────────────────────

/// Persisted onboarding state, stored as `~/.bitrouter/onboarding.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingState {
    /// Current status of the onboarding process.
    pub status: OnboardingStatus,
    /// Prefix of the active keypair used during onboarding (if any).
    /// Links to `~/.bitrouter/.keys/<prefix>/master.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keypair_prefix: Option<String>,
}

/// Discrete onboarding outcomes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingStatus {
    /// Onboarding has never been started.
    NotStarted,
    /// User completed onboarding with BitRouter Node (MPP payments).
    CompletedNode,
    /// User chose to bring their own API keys (skipped node onboarding).
    CompletedByok,
    /// User deferred onboarding (e.g., Ctrl-C or explicit skip).
    Deferred,
    /// Onboarding failed but can be retried.
    FailedRecoverable,
}

impl OnboardingState {
    pub fn new() -> Self {
        Self {
            status: OnboardingStatus::NotStarted,
            keypair_prefix: None,
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────

/// Path to the onboarding state file.
pub fn state_file(home_dir: &Path) -> std::path::PathBuf {
    home_dir.join("onboarding.json")
}

/// Load onboarding state from disk. Returns `NotStarted` if file is missing.
pub fn load_state(home_dir: &Path) -> OnboardingState {
    let path = state_file(home_dir);
    match fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_else(|_| OnboardingState::new()),
        Err(_) => OnboardingState::new(),
    }
}

/// Persist onboarding state to disk.
pub fn save_state(home_dir: &Path, state: &OnboardingState) -> Result<(), String> {
    let path = state_file(home_dir);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize onboarding state: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

// ── Onboarding detection ──────────────────────────────────────

/// Returns `true` if onboarding should be triggered.
///
/// Onboarding runs when:
/// - The state is `NotStarted` (first run), OR
/// - The state is `Deferred` (user skipped previously, re-prompt once).
pub fn should_onboard(home_dir: &Path) -> bool {
    let state = load_state(home_dir);
    matches!(
        state.status,
        OnboardingStatus::NotStarted | OnboardingStatus::Deferred
    )
}

// ── Interactive onboarding flow ───────────────────────────────

/// Outcome of the interactive onboarding flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingOutcome {
    /// User completed the node onboarding path.
    CompletedNode {
        /// Public-key prefix of the active keypair.
        keypair_prefix: String,
    },
    /// User chose BYOK (bring your own keys).
    CompletedByok,
    /// User deferred (Ctrl-C, explicit skip).
    Deferred,
}

/// Run the interactive onboarding flow.
///
/// Generates (or reuses) a web3 master keypair for MPP payments on Tempo,
/// shows the EVM wallet address, and instructs the user to fund it.
pub fn run_onboarding(home_dir: &Path) -> Result<OnboardingOutcome, Box<dyn std::error::Error>> {
    let theme = ColorfulTheme::default();

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("Onboarding requires an interactive terminal.");
        eprintln!("Run `bitrouter init` manually, or skip with BYOK.");
        let mut state = load_state(home_dir);
        state.status = OnboardingStatus::Deferred;
        save_state(home_dir, &state)?;
        return Ok(OnboardingOutcome::Deferred);
    }

    println!();
    println!("  BitRouter Node Onboarding");
    println!("  ────────────────────────");
    println!();
    println!("  BitRouter Node uses MPP (Machine Payment Protocol) on Tempo");
    println!("  to pay for LLM requests. A web3 wallet will be generated for");
    println!("  you, or you can skip and bring your own API keys (BYOK).");
    println!();

    // ── Keypair setup ───────────────────────────────────────────
    let keys_dir = home_dir.join(".keys");
    let prefix = match account::load_active_keypair(&keys_dir) {
        Ok((existing_prefix, kp)) => {
            let evm_addr = kp
                .evm_address_string()
                .map_err(|e| format!("failed to derive EVM address: {e}"))?;
            let sol_addr = kp.solana_pubkey_b58();
            println!("  Existing wallet found:");
            println!("    evm:    {evm_addr}");
            println!("    solana: {sol_addr}");
            println!("    prefix: {existing_prefix}");
            println!();

            let choices = &[
                "Use this wallet",
                "Generate a new wallet",
                "Skip \u{2014} I'll bring my own API keys (BYOK)",
            ];

            let selection = Select::with_theme(&theme)
                .with_prompt("Wallet setup")
                .items(choices)
                .default(0)
                .interact()?;

            match selection {
                0 => existing_prefix,
                1 => generate_keypair(&keys_dir)?,
                _ => return complete_byok(home_dir),
            }
        }
        Err(_) => {
            let choices = &[
                "Generate a new wallet",
                "Skip \u{2014} I'll bring my own API keys (BYOK)",
            ];

            let selection = Select::with_theme(&theme)
                .with_prompt("No wallet found. Choose an option")
                .items(choices)
                .default(0)
                .interact()?;

            match selection {
                0 => generate_keypair(&keys_dir)?,
                _ => return complete_byok(home_dir),
            }
        }
    };

    // ── Show funding instructions ───────────────────────────────
    let (_, kp) = account::load_active_keypair(&keys_dir)
        .map_err(|e| format!("failed to reload keypair: {e}"))?;
    let evm_addr = kp
        .evm_address_string()
        .map_err(|e| format!("failed to derive EVM address: {e}"))?;

    println!();
    println!("  ✓ Wallet ready!");
    println!();
    println!("  Fund your EVM wallet on Tempo to start making requests:");
    println!("    Address: {evm_addr}");
    println!("    Fund at: https://app.tempo.xyz");
    println!();

    // ── Save state ──────────────────────────────────────────────
    let mut state = load_state(home_dir);
    state.keypair_prefix = Some(prefix.clone());
    state.status = OnboardingStatus::CompletedNode;
    save_state(home_dir, &state)?;

    println!("  ✓ Onboarding complete!");
    println!();

    Ok(OnboardingOutcome::CompletedNode {
        keypair_prefix: prefix,
    })
}

/// Generate a new keypair and set it as active. Returns the prefix.
fn generate_keypair(keys_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    use bitrouter_core::auth::keys::MasterKeypair;

    let kp = MasterKeypair::generate();
    let prefix = kp.public_key_prefix();
    let sol_addr = kp.solana_pubkey_b58();
    let evm_addr = kp
        .evm_address_string()
        .map_err(|e| format!("failed to derive EVM address: {e}"))?;

    let key_dir = keys_dir.join(&prefix);
    fs::create_dir_all(&key_dir).map_err(|e| format!("failed to create key directory: {e}"))?;

    let json = kp.to_json();
    let json_str =
        serde_json::to_string_pretty(&json).map_err(|e| format!("failed to serialize key: {e}"))?;
    fs::write(key_dir.join("master.json"), json_str)
        .map_err(|e| format!("failed to write master.json: {e}"))?;

    // Create tokens directory for this account.
    fs::create_dir_all(key_dir.join("tokens"))
        .map_err(|e| format!("failed to create tokens directory: {e}"))?;

    // Set as active.
    fs::write(keys_dir.join("active"), &prefix)
        .map_err(|e| format!("failed to write active file: {e}"))?;

    println!("  Generated web3 master key:");
    println!("    evm:    {evm_addr}");
    println!("    solana: {sol_addr}");
    println!("    prefix: {prefix}");

    Ok(prefix)
}

/// Complete onboarding with BYOK status.
fn complete_byok(home_dir: &Path) -> Result<OnboardingOutcome, Box<dyn std::error::Error>> {
    let mut state = load_state(home_dir);
    state.status = OnboardingStatus::CompletedByok;
    save_state(home_dir, &state)?;
    println!();
    println!("  Skipped node onboarding. Configure providers manually:");
    println!("    bitrouter init");
    println!();
    Ok(OnboardingOutcome::CompletedByok)
}
