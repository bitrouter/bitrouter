//! `bitrouter agent-proxy` subcommand — runs an ACP stdio proxy for
//! a configured agent, injecting auth, spend tracking, and rate limiting.
//!
//! The consumer (e.g. an editor like Zed) spawns
//! `bitrouter agent-proxy <agent-name>` and speaks ACP over stdin/stdout.

use std::sync::Arc;

use bitrouter_config::BitrouterConfig;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use bitrouter_providers::acp::proxy::{ProxyConfig, run_stdio_proxy};

/// Run the agent-proxy command.
///
/// Loads the agent configuration, resolves the operator wallet for JWT
/// verification, constructs the upstream `AcpAgentProvider`, and hands
/// off to the stdio proxy loop.
pub fn run(
    config: &BitrouterConfig,
    agent_name: &str,
    token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Merge user-configured agents with built-in definitions.
    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let agent_config = known
        .get(agent_name)
        .ok_or_else(|| format!("unknown agent: {agent_name}"))?
        .clone();

    if !agent_config.enabled {
        return Err(format!("agent '{agent_name}' is disabled in configuration").into());
    }

    // Resolve operator CAIP-10 for JWT issuer verification.
    let operator_caip10 = config.wallet.as_ref().and_then(|wallet| {
        resolve_operator_caip10(wallet)
            .map_err(|e| {
                eprintln!("warning: could not resolve operator CAIP-10: {e}");
            })
            .ok()
    });

    let provider = Arc::new(AcpAgentProvider::new(agent_name.to_owned(), agent_config));

    let proxy_config = ProxyConfig {
        agent_name: agent_name.to_owned(),
        pre_auth_token: token.map(str::to_owned),
        operator_caip10,
        observer: None,
    };

    // Run the proxy on the current thread (blocks until consumer disconnects).
    run_stdio_proxy(provider, proxy_config).map_err(|e| e.into())
}

/// Resolve the operator's CAIP-10 identity from wallet configuration.
///
/// Duplicates the logic from `runtime::server` since that function is
/// private and tightly coupled to the serve path.
fn resolve_operator_caip10(
    wallet: &bitrouter_config::config::WalletConfig,
) -> Result<String, String> {
    use bitrouter_core::auth::chain::{Caip10, Chain};

    let vault = wallet.vault_path.as_deref().map(std::path::Path::new);
    let info = ows_lib::get_wallet(&wallet.name, vault)
        .map_err(|e| format!("failed to load wallet '{}': {e}", wallet.name))?;

    let sol_account = info
        .accounts
        .iter()
        .find(|a| a.chain_id.starts_with("solana:"))
        .ok_or_else(|| format!("wallet '{}' has no Solana account", wallet.name))?;

    let caip10 = Caip10 {
        chain: Chain::solana_mainnet(),
        address: sol_account.address.clone(),
    };

    Ok(caip10.format())
}
