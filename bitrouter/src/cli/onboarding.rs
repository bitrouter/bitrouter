//! First-run onboarding flow for BitRouter cloud node.
//!
//! Auto-triggered on first `serve` / `start` when no onboarding state marker
//! exists. Guides the user through wallet setup and cloud provider config.

use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::{Input, Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

use crate::cli::swig;

// ── Default Solana wallet path ────────────────────────────────

const DEFAULT_SOLANA_WALLET: &str = ".config/solana/id.json";

// ── Onboarding state model ────────────────────────────────────

/// Persisted onboarding state, stored as `~/.bitrouter/onboarding.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingState {
    /// Current status of the onboarding process.
    pub status: OnboardingStatus,
    /// Path to the master wallet file used during onboarding (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_wallet_path: Option<PathBuf>,
    /// Solana RPC URL chosen during onboarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
}

/// Discrete onboarding outcomes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingStatus {
    /// Onboarding has never been started.
    NotStarted,
    /// User completed onboarding with BitRouter cloud node.
    CompletedCloud,
    /// User chose to bring their own API keys (skipped cloud onboarding).
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
            master_wallet_path: None,
            rpc_url: None,
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────

/// Path to the onboarding state file.
pub fn state_file(home_dir: &Path) -> PathBuf {
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
    /// User completed the cloud onboarding path.
    CompletedCloud {
        /// Solana RPC URL chosen during onboarding.
        rpc_url: String,
    },
    /// User chose BYOK (bring your own keys).
    CompletedByok,
    /// User deferred (Ctrl-C, explicit skip).
    Deferred,
}

/// Run the interactive onboarding flow.
///
/// Guides the user through wallet selection and RPC URL configuration.
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
    println!("  BitRouter Cloud Node Onboarding");
    println!("  ───────────────────────────────");
    println!();
    println!("  BitRouter Cloud uses x402 for request payments.");
    println!("  You need a Solana wallet to pay for agent requests,");
    println!("  or you can skip and bring your own API keys (BYOK).");
    println!();

    // ── Step 1: Wallet selection ────────────────────────────────
    let wallet_path = match prompt_wallet_selection(home_dir, &theme)? {
        WalletChoice::UseDefault(path) => Some(path),
        WalletChoice::Import(path) => Some(path),
        WalletChoice::Create(path) => Some(path),
        WalletChoice::SkipByok => {
            let mut state = load_state(home_dir);
            state.status = OnboardingStatus::CompletedByok;
            save_state(home_dir, &state)?;
            println!();
            println!("  Skipped cloud onboarding. Configure providers manually:");
            println!("    bitrouter init");
            println!();
            return Ok(OnboardingOutcome::CompletedByok);
        }
    };

    let wallet_path = wallet_path.ok_or("no wallet path selected")?;

    // ── Step 2: Solana RPC URL ──────────────────────────────────
    println!();
    let rpc_url: String = Input::with_theme(&theme)
        .with_prompt("Solana RPC URL")
        .default(swig::DEFAULT_RPC_URL.to_string())
        .interact_text()?;

    // ── Step 3: Save state ──────────────────────────────────────
    let mut state = load_state(home_dir);
    state.master_wallet_path = Some(wallet_path);
    state.rpc_url = Some(rpc_url.clone());
    state.status = OnboardingStatus::CompletedCloud;
    save_state(home_dir, &state)?;

    println!();
    println!("  ✓ Onboarding complete!");
    println!();

    Ok(OnboardingOutcome::CompletedCloud { rpc_url })
}

// ── Wallet selection ──────────────────────────────────────────

enum WalletChoice {
    UseDefault(PathBuf),
    Import(PathBuf),
    Create(PathBuf),
    SkipByok,
}

fn prompt_wallet_selection(
    home_dir: &Path,
    theme: &ColorfulTheme,
) -> Result<WalletChoice, Box<dyn std::error::Error>> {
    let default_wallet = dirs::home_dir()
        .map(|h| h.join(DEFAULT_SOLANA_WALLET))
        .filter(|p| p.exists());

    if let Some(ref default_path) = default_wallet {
        println!(
            "  Detected default Solana wallet: {}",
            default_path.display()
        );
        println!();

        let choices = &[
            "Use this wallet as master wallet",
            "Import a different wallet file",
            "Create a new wallet",
            "Skip \u{2014} I'll bring my own API keys (BYOK)",
        ];

        let selection = Select::with_theme(theme)
            .with_prompt("Choose wallet setup")
            .items(choices)
            .default(0)
            .interact()?;

        match selection {
            0 => Ok(WalletChoice::UseDefault(default_path.clone())),
            1 => prompt_import_wallet(theme),
            2 => prompt_create_wallet(home_dir, theme),
            _ => Ok(WalletChoice::SkipByok),
        }
    } else {
        println!("  No default Solana wallet found (~/.config/solana/id.json).");
        println!();

        let choices = &[
            "Import an existing wallet file",
            "Create a new wallet",
            "Skip \u{2014} I'll bring my own API keys (BYOK)",
        ];

        let selection = Select::with_theme(theme)
            .with_prompt("Choose wallet setup")
            .items(choices)
            .default(1)
            .interact()?;

        match selection {
            0 => prompt_import_wallet(theme),
            1 => prompt_create_wallet(home_dir, theme),
            _ => Ok(WalletChoice::SkipByok),
        }
    }
}

fn prompt_import_wallet(theme: &ColorfulTheme) -> Result<WalletChoice, Box<dyn std::error::Error>> {
    let path_str: String = Input::with_theme(theme)
        .with_prompt("Path to Solana wallet JSON file")
        .interact_text()?;

    let path = expand_tilde(&path_str);
    if !path.exists() {
        return Err(format!("wallet file not found: {}", path.display()).into());
    }

    Ok(WalletChoice::Import(path))
}

fn prompt_create_wallet(
    home_dir: &Path,
    theme: &ColorfulTheme,
) -> Result<WalletChoice, Box<dyn std::error::Error>> {
    let default_path = home_dir.join("wallet.json");

    let path_str: String = Input::with_theme(theme)
        .with_prompt("Save new wallet to")
        .default(default_path.display().to_string())
        .interact_text()?;

    let path = expand_tilde(&path_str);

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create directory {}: {e}", parent.display()))?;
    }

    Ok(WalletChoice::Create(path))
}

/// Expand leading `~` to the user's home directory.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}
