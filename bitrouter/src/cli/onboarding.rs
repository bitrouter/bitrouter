//! First-run onboarding flow for BitRouter cloud node.
//!
//! Auto-triggered on first `serve` / `start` when no onboarding state marker
//! exists. Guides the user through wallet setup, embedded wallet creation via
//! Swig, optional agent wallet derivation, and cloud provider default config.

use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
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
    /// Embedded Swig wallet address (PDA).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_wallet_address: Option<String>,
    /// Embedded Swig wallet address for receiving funds (PDA).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    /// Hex-encoded 32-byte Swig wallet ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swig_id: Option<String>,
    /// Solana RPC URL chosen during onboarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// Next Swig role ID to assign when deriving an agent wallet.
    /// Master is always role 0; agents start at 1 and increment.
    #[serde(default = "default_next_role_id")]
    pub next_role_id: u32,
    /// Agent wallets derived from the embedded wallet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_wallets: Vec<AgentWalletState>,
}

fn default_next_role_id() -> u32 {
    1
}

/// Persisted agent wallet reference data (for display only; Swig is source of truth).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWalletState {
    /// Human-readable label (e.g. "default", "research-agent").
    pub label: String,
    /// On-chain public key of the agent authority.
    pub address: String,
    /// Swig role ID for this agent.
    pub role_id: u32,
    /// Spend permissions.
    pub permissions: swig::AgentPermissions,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
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
            embedded_wallet_address: None,
            wallet_address: None,
            swig_id: None,
            rpc_url: None,
            next_role_id: 1,
            agent_wallets: Vec::new(),
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

/// Add an agent wallet entry to the persisted onboarding state.
pub fn add_agent_wallet(home_dir: &Path, agent: AgentWalletState) -> Result<(), String> {
    let mut state = load_state(home_dir);
    // Replace if same label already exists.
    state.agent_wallets.retain(|a| a.label != agent.label);
    state.agent_wallets.push(agent);
    save_state(home_dir, &state)
}

/// Load all agent wallets from onboarding state.
pub fn load_agent_wallets(home_dir: &Path) -> Vec<AgentWalletState> {
    load_state(home_dir).agent_wallets
}

/// Load the first (default) agent wallet, if any.
pub fn load_agent_wallet(home_dir: &Path) -> Option<AgentWalletState> {
    load_state(home_dir).agent_wallets.into_iter().next()
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
/// Returns the outcome so the caller can decide whether to write cloud
/// provider config or skip.
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

    // ── Step 3: Create embedded wallet ──────────────────────────
    println!();
    println!("  Creating Swig embedded wallet...");
    match swig::create_embedded_wallet(&wallet_path, &rpc_url) {
        Ok(info) => {
            println!("  ✓ Embedded wallet created: {}", info.address);
            println!();
            println!("  ┌─────────────────────────────────────────────────┐");
            println!("  │  Fund your wallet to start making requests      │");
            println!("  │                                                 │");
            println!("  │  Send SOL (for tx fees) and USDC (for payments) │");
            println!("  │  to the address below:                          │");
            println!("  │                                                 │");
            println!("  │  {:<47} │", info.wallet_address);
            println!("  └─────────────────────────────────────────────────┘");
            println!();

            let mut state = load_state(home_dir);
            state.master_wallet_path = Some(wallet_path.clone());
            state.embedded_wallet_address = Some(info.address);
            state.wallet_address = Some(info.wallet_address);
            state.swig_id = Some(info.swig_id);
            state.rpc_url = Some(rpc_url.clone());
            save_state(home_dir, &state)?;
        }
        Err(e) => {
            eprintln!("  ✗ Failed to create embedded wallet: {e}");
            eprintln!("  You can retry later with: bitrouter sudo create-embedded-wallet");
            let mut state = load_state(home_dir);
            state.status = OnboardingStatus::FailedRecoverable;
            state.master_wallet_path = Some(wallet_path);
            state.rpc_url = Some(rpc_url);
            save_state(home_dir, &state)?;
            return Ok(OnboardingOutcome::Deferred);
        }
    }

    // ── Step 4: Optional agent wallet derivation ────────────────
    println!();
    let derive_agent = Confirm::with_theme(&theme)
        .with_prompt("Derive an agent wallet now? (can be done later with `bitrouter sudo derive-agent-wallet`)")
        .default(true)
        .interact()?;

    if derive_agent {
        let swig_account = load_state(home_dir)
            .embedded_wallet_address
            .ok_or("embedded wallet address missing")?;
        match prompt_and_derive_agent(home_dir, &wallet_path, &rpc_url, &swig_account, &theme)? {
            Some(agent) => {
                println!(
                    "  ✓ Agent wallet derived: {} ({})",
                    agent.address, agent.label
                );
            }
            None => {
                println!("  Agent wallet derivation skipped or failed.");
            }
        }
    }

    // ── Step 6: Mark complete ───────────────────────────────────
    let mut state = load_state(home_dir);
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
            "Create a new wallet (recommended: set a passphrase)",
            "Skip — I'll bring my own API keys (BYOK)",
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
            "Create a new wallet (recommended: set a passphrase)",
            "Skip — I'll bring my own API keys (BYOK)",
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
    println!();
    println!("  A passphrase protects your wallet from unauthorized agent access.");
    println!("  This is like a \"sudo password\" — you'll enter it when signing.");
    println!("  Leave empty to skip passphrase protection.");
    println!();

    // We inform the user but actual wallet creation is delegated to Swig.
    // For now, just ask where to save.
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

    // Actual keypair creation will happen in the Swig integration.
    // For now we just record the intended path.
    Ok(WalletChoice::Create(path))
}

// ── Agent wallet derivation prompts ───────────────────────────

fn prompt_and_derive_agent(
    home_dir: &Path,
    wallet_path: &Path,
    rpc_url: &str,
    swig_account: &str,
    theme: &ColorfulTheme,
) -> Result<Option<AgentWalletState>, Box<dyn std::error::Error>> {
    println!();
    println!("  Configure agent wallet spend limits (enforced by Swig on-chain).");
    println!("  Leave empty for no limit.");
    println!();

    let label: String = Input::with_theme(theme)
        .with_prompt("Agent label")
        .default("default".into())
        .interact_text()?;

    let per_tx_cap: String = Input::with_theme(theme)
        .with_prompt("Per-transaction cap (lamports, empty = unlimited)")
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;
    let per_tx_cap = parse_optional_u64(&per_tx_cap)?;

    let cumulative_cap: String = Input::with_theme(theme)
        .with_prompt("Cumulative spending cap (lamports, empty = unlimited)")
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;
    let cumulative_cap = parse_optional_u64(&cumulative_cap)?;

    let expiration: String = Input::with_theme(theme)
        .with_prompt("Expiration (e.g., \"7d\", \"30d\", \"never\", empty = never)")
        .default("30d".into())
        .interact_text()?;
    let expires_at = parse_expiration_input(&expiration)?;

    let permissions = swig::AgentPermissions {
        per_tx_cap,
        cumulative_cap,
        expires_at,
    };

    println!();
    println!("  Deriving agent wallet with Swig...");
    let current_state = load_state(home_dir);
    let role_id = current_state.next_role_id;
    match swig::derive_agent_wallet(
        wallet_path,
        &permissions,
        rpc_url,
        &label,
        home_dir,
        swig_account,
        role_id,
    ) {
        Ok((info, _keypair_bytes)) => {
            let agent = AgentWalletState {
                label: info.label.clone(),
                address: info.address,
                role_id: info.role_id,
                permissions: info.permissions,
                created_at: info.created_at,
            };
            add_agent_wallet(home_dir, agent.clone())?;
            // Increment role_id for the next agent wallet.
            let mut state = load_state(home_dir);
            state.next_role_id = role_id + 1;
            save_state(home_dir, &state)?;
            Ok(Some(agent))
        }
        Err(e) => {
            eprintln!("  ✗ Failed to derive agent wallet: {e}");
            eprintln!("  You can retry later with: bitrouter sudo derive-agent-wallet");
            Ok(None)
        }
    }
}

fn parse_optional_u64(s: &str) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<u64>()
        .map(Some)
        .map_err(|e| format!("invalid number \"{s}\": {e}").into())
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

fn parse_expiration_input(s: &str) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let s = s.trim();
    if s.is_empty() || s == "never" {
        return Ok(None);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_secs();

    // Parse relative durations: "7d", "30d", "1h", "365d"
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

    // Try as absolute UNIX timestamp
    if let Ok(ts) = s.parse::<u64>() {
        return Ok(Some(ts));
    }

    Err(
        format!("invalid expiration \"{s}\" — use \"7d\", \"30d\", \"never\", or a UNIX timestamp")
            .into(),
    )
}
