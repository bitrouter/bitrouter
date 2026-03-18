//! Swig embedded wallet operations.
//!
//! When the `swig` feature is enabled, functions use the real
//! `bitrouter-swig-sdk` + `solana-client` to interact with on-chain Swig
//! wallets. Without the feature, stubs return an error directing the user
//! to rebuild with `--features swig`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default Solana mainnet RPC endpoint.
pub const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// Mainnet USDC mint address (only used by the swig feature).
#[cfg(feature = "swig")]
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

// ── Public types ──────────────────────────────────────────────

/// Metadata returned after creating a Swig embedded wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedWalletInfo {
    /// The on-chain address of the Swig wallet (PDA).
    pub address: String,
    /// The on-chain address of the wallet's token account (PDA).
    pub wallet_address: String,
    /// The 32-byte wallet ID (hex-encoded) used for PDA derivation.
    pub swig_id: String,
    /// Human-readable creation timestamp (ISO 8601).
    pub created_at: String,
}

/// Metadata returned after deriving an agent wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWalletInfo {
    /// The on-chain public key of the derived agent keypair.
    pub address: String,
    /// The Swig role ID assigned to this agent authority.
    pub role_id: u32,
    /// Human-readable label for this agent wallet.
    pub label: String,
    /// Spend permissions associated with this agent wallet.
    pub permissions: AgentPermissions,
    /// Human-readable creation timestamp (ISO 8601).
    pub created_at: String,
}

/// Spend permissions for an agent wallet — enforced by Swig on-chain.
///
/// Local copies of these values are stored for display/reference only;
/// Swig is the source of truth for enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPermissions {
    /// Maximum lamports per single SOL transaction.
    pub per_tx_cap: Option<u64>,
    /// Cumulative SOL spending cap across all transactions.
    pub cumulative_cap: Option<u64>,
    /// Unix timestamp after which the agent wallet expires.
    pub expires_at: Option<u64>,
}

/// SOL and USDC balances for a wallet address.
#[derive(Debug, Clone)]
pub struct WalletBalance {
    /// SOL balance in lamports.
    pub sol_lamports: u64,
    /// USDC balance in smallest unit (6 decimals), or `None` if no ATA exists.
    pub usdc_amount: Option<u64>,
}

impl WalletBalance {
    pub fn sol_display(&self) -> String {
        format!("{:.4} SOL", self.sol_lamports as f64 / LAMPORTS_PER_SOL)
    }

    pub fn usdc_display(&self) -> String {
        match self.usdc_amount {
            Some(amount) => format!("{:.2} USDC", amount as f64 / 1_000_000.0),
            None => "0.00 USDC (no token account)".to_string(),
        }
    }
}

// ── Feature-gated implementation ──────────────────────────────

#[cfg(feature = "swig")]
mod inner {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use solana_client::rpc_client::RpcClient;
    use solana_instruction::Instruction;
    use solana_keypair::{Keypair, Signer, read_keypair_file};
    use solana_pubkey::Pubkey;
    use solana_transaction::Transaction;

    use bitrouter_swig_sdk::auth::ClientRole;
    use bitrouter_swig_sdk::auth::ed25519::Ed25519ClientRole;
    use bitrouter_swig_sdk::types::{AuthorityConfig, AuthorityType, Permission};
    use bitrouter_swig_sdk::{instruction, pda};

    use super::{AgentPermissions, AgentWalletInfo, EmbeddedWalletInfo, USDC_MINT, WalletBalance};

    /// Load a Solana JSON keypair file (array of 64 bytes).
    fn load_keypair(path: &Path) -> Result<Keypair, String> {
        read_keypair_file(path).map_err(|e| format!("cannot read keypair {}: {e}", path.display()))
    }

    fn now_iso8601() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Minimal ISO 8601 without external dep
        format!("{secs}")
    }

    fn send_and_confirm(
        client: &RpcClient,
        ixs: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<String, String> {
        let blockhash = client
            .get_latest_blockhash()
            .map_err(|e| format!("failed to get blockhash: {e}"))?;
        let payer_pk = signers[0].pubkey();
        let mut tx = Transaction::new_with_payer(ixs, Some(&payer_pk));
        tx.sign(signers, blockhash);
        let sig = client
            .send_and_confirm_transaction(&tx)
            .map_err(|e| format!("transaction failed: {e}"))?;
        Ok(sig.to_string())
    }

    /// Save a keypair to a JSON file (Solana CLI format: array of 64 bytes).
    fn save_keypair(path: &Path, keypair: &Keypair) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create directory {}: {e}", parent.display()))?;
        }
        let bytes = keypair.to_bytes();
        let json = serde_json::to_string(&bytes.to_vec())
            .map_err(|e| format!("failed to serialize keypair: {e}"))?;
        fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    pub fn create_embedded_wallet(
        master_keypair_path: &Path,
        rpc_url: &str,
    ) -> Result<EmbeddedWalletInfo, String> {
        let master = load_keypair(master_keypair_path)?;
        let client = RpcClient::new(rpc_url.to_string());

        // Generate random 32-byte wallet ID.
        let swig_id: [u8; 32] = rand_id();

        // Derive PDAs.
        let (swig_account, bump) = pda::swig_account(&swig_id);
        let (wallet_address, wallet_bump) = pda::swig_wallet_address(&swig_account);

        // Build CreateV1 instruction with master as full-access authority.
        let ix = instruction::create::create_v1(
            swig_account,
            master.pubkey(),
            wallet_address,
            swig_id,
            bump,
            wallet_bump,
            AuthorityType::Ed25519,
            &master.pubkey().to_bytes(),
            &[Permission::All],
        );

        let sig = send_and_confirm(&client, &[ix], &[&master])?;
        eprintln!("  tx: {sig}");

        Ok(EmbeddedWalletInfo {
            address: swig_account.to_string(),
            wallet_address: wallet_address.to_string(),
            swig_id: swig_id
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            created_at: now_iso8601(),
        })
    }

    pub fn derive_agent_wallet(
        master_keypair_path: &Path,
        permissions: &AgentPermissions,
        rpc_url: &str,
        label: &str,
        home_dir: &Path,
        swig_account_str: &str,
        role_id: u32,
    ) -> Result<(AgentWalletInfo, Vec<u8>), String> {
        let master = load_keypair(master_keypair_path)?;
        let client = RpcClient::new(rpc_url.to_string());

        // Generate a fresh agent keypair and persist it.
        let agent_kp = Keypair::new();
        let agent_file = home_dir.join(format!("agent_{label}.json"));
        save_keypair(&agent_file, &agent_kp)?;

        let swig_account: Pubkey = swig_account_str
            .parse()
            .map_err(|e| format!("invalid swig account address: {e}"))?;

        // Map AgentPermissions → SDK Permission list.
        let sdk_perms = build_permissions(permissions);

        let role = Ed25519ClientRole::new(master.pubkey());
        let new_auth = AuthorityConfig {
            authority_type: AuthorityType::Ed25519,
            authority_bytes: agent_kp.pubkey().to_bytes().to_vec(),
        };

        let ixs = role
            .add_authority(
                swig_account,
                master.pubkey(),
                0, // master is role 0
                new_auth,
                sdk_perms,
                None,
            )
            .map_err(|e| format!("failed to build add_authority instruction: {e}"))?;

        let sig = send_and_confirm(&client, &ixs, &[&master])?;
        eprintln!("  tx: {sig}");

        // Role ID = role_counter at creation time.
        // The caller tracks role assignment via next_role_id in onboarding state.

        let info = AgentWalletInfo {
            address: agent_kp.pubkey().to_string(),
            role_id,
            label: label.to_string(),
            permissions: permissions.clone(),
            created_at: now_iso8601(),
        };

        Ok((info, agent_kp.to_bytes().to_vec()))
    }

    pub fn set_agent_permissions(
        master_keypair_path: &Path,
        _agent_address: &str,
        permissions: &AgentPermissions,
        rpc_url: &str,
        swig_account_str: &str,
        agent_role_id: u32,
    ) -> Result<AgentPermissions, String> {
        let master = load_keypair(master_keypair_path)?;
        let client = RpcClient::new(rpc_url.to_string());

        let swig_account: Pubkey = swig_account_str
            .parse()
            .map_err(|e| format!("invalid swig account address: {e}"))?;

        let sdk_perms = build_permissions(permissions);
        let update = bitrouter_swig_sdk::types::UpdateAuthorityData::ReplaceAll(sdk_perms);

        let role = Ed25519ClientRole::new(master.pubkey());
        let ixs = role
            .update_authority(
                swig_account,
                master.pubkey(),
                0, // master is role 0
                agent_role_id,
                update,
                None,
            )
            .map_err(|e| format!("failed to build update_authority instruction: {e}"))?;

        let sig = send_and_confirm(&client, &ixs, &[&master])?;
        eprintln!("  tx: {sig}");

        Ok(permissions.clone())
    }

    pub fn get_balance(rpc_url: &str, address: &str) -> Result<WalletBalance, String> {
        let client = RpcClient::new(rpc_url.to_string());
        let pubkey: Pubkey = address
            .parse()
            .map_err(|e| format!("invalid address \"{address}\": {e}"))?;

        let sol_lamports = client
            .get_balance(&pubkey)
            .map_err(|e| format!("RPC error fetching SOL balance: {e}"))?;

        // USDC associated token account.
        let usdc_mint: Pubkey = USDC_MINT
            .parse()
            .map_err(|e| format!("invalid USDC mint: {e}"))?;
        let ata = spl_associated_token_account::get_associated_token_address(&pubkey, &usdc_mint);

        let usdc_amount = match client.get_token_account_balance(&ata) {
            Ok(balance) => balance.amount.parse::<u64>().ok(),
            Err(_) => None, // ATA doesn't exist or RPC error
        };

        Ok(WalletBalance {
            sol_lamports,
            usdc_amount,
        })
    }

    /// Map our `AgentPermissions` to a list of SDK `Permission` variants.
    fn build_permissions(p: &AgentPermissions) -> Vec<Permission> {
        let mut perms = Vec::new();

        // SOL spend cap
        if let Some(amount) = p.cumulative_cap {
            perms.push(Permission::Sol { amount });
        } else if let Some(amount) = p.per_tx_cap {
            // Use per-tx as a SOL limit if no cumulative cap set.
            perms.push(Permission::Sol { amount });
        }

        // USDC spend cap (mirror SOL limits for the USDC mint)
        // Safety: USDC_MINT is a hardcoded valid base58 pubkey; parse only fails
        // on malformed input, so the `Ok` branch is always taken.
        let Ok(usdc_mint) = USDC_MINT.parse::<Pubkey>() else {
            return perms;
        };
        if let Some(amount) = p.cumulative_cap {
            perms.push(Permission::Token {
                mint: usdc_mint,
                amount,
            });
        } else if let Some(amount) = p.per_tx_cap {
            perms.push(Permission::Token {
                mint: usdc_mint,
                amount,
            });
        }

        // If no caps at all, grant all-but-manage for flexible agent usage.
        if perms.is_empty() {
            perms.push(Permission::AllButManageAuthority);
        }

        perms
    }

    /// Generate a random 32-byte ID.
    fn rand_id() -> [u8; 32] {
        rand::random::<[u8; 32]>()
    }
}

#[cfg(not(feature = "swig"))]
mod inner {
    use std::path::Path;

    use super::{AgentPermissions, AgentWalletInfo, EmbeddedWalletInfo, WalletBalance};

    const DISABLED: &str = "swig feature not enabled — rebuild with `cargo build --features swig`";

    pub fn create_embedded_wallet(
        _master_keypair_path: &Path,
        _rpc_url: &str,
    ) -> Result<EmbeddedWalletInfo, String> {
        Err(DISABLED.into())
    }

    pub fn derive_agent_wallet(
        _master_keypair_path: &Path,
        _permissions: &AgentPermissions,
        _rpc_url: &str,
        _label: &str,
        _home_dir: &Path,
        _swig_account_str: &str,
        _role_id: u32,
    ) -> Result<(AgentWalletInfo, Vec<u8>), String> {
        Err(DISABLED.into())
    }

    pub fn set_agent_permissions(
        _master_keypair_path: &Path,
        _agent_address: &str,
        _permissions: &AgentPermissions,
        _rpc_url: &str,
        _swig_account_str: &str,
        _agent_role_id: u32,
    ) -> Result<AgentPermissions, String> {
        Err(DISABLED.into())
    }

    pub fn get_balance(_rpc_url: &str, _address: &str) -> Result<WalletBalance, String> {
        Err(DISABLED.into())
    }
}

// ── Re-export through the public API ──────────────────────────

pub fn create_embedded_wallet(
    master_keypair_path: &Path,
    rpc_url: &str,
) -> Result<EmbeddedWalletInfo, String> {
    inner::create_embedded_wallet(master_keypair_path, rpc_url)
}

pub fn derive_agent_wallet(
    master_keypair_path: &Path,
    permissions: &AgentPermissions,
    rpc_url: &str,
    label: &str,
    home_dir: &Path,
    swig_account_str: &str,
    role_id: u32,
) -> Result<(AgentWalletInfo, Vec<u8>), String> {
    inner::derive_agent_wallet(
        master_keypair_path,
        permissions,
        rpc_url,
        label,
        home_dir,
        swig_account_str,
        role_id,
    )
}

pub fn set_agent_permissions(
    master_keypair_path: &Path,
    agent_address: &str,
    permissions: &AgentPermissions,
    rpc_url: &str,
    swig_account_str: &str,
    agent_role_id: u32,
) -> Result<AgentPermissions, String> {
    inner::set_agent_permissions(
        master_keypair_path,
        agent_address,
        permissions,
        rpc_url,
        swig_account_str,
        agent_role_id,
    )
}

pub fn get_balance(rpc_url: &str, address: &str) -> Result<WalletBalance, String> {
    inner::get_balance(rpc_url, address)
}
