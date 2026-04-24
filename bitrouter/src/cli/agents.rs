//! `bitrouter agents` subcommand — list, install, uninstall, update, and
//! check ACP agents.

use bitrouter_config::BitrouterConfig;
use bitrouter_config::acp::registry_agent_to_config;
use bitrouter_providers::acp::eager;
use bitrouter_providers::acp::registry;
use bitrouter_providers::acp::state;
use bitrouter_providers::acp::types::InstallProgress;
use tokio::sync::mpsc;

use crate::runtime::paths::RuntimePaths;

/// Run the `agents list` subcommand — prints every agent available
/// across (a) the config, (b) the ACP registry, and (c) installed
/// ledger, plus PATH availability.
pub async fn run_list(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    refresh: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cache_file = paths.cache_dir.join("acp-registry.json");
    let registry_url = registry::resolve_registry_url(config.acp_registry_url.as_deref());

    let registry_result = if refresh {
        registry::fetch_registry_fresh(&cache_file, &registry_url).await
    } else {
        registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &registry_url).await
    };

    // Merge: registry > config > built-in.
    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let registry_map = match &registry_result {
        Ok(index) => {
            let mut map = std::collections::HashMap::new();
            for agent in &index.agents {
                known.insert(agent.id.clone(), registry_agent_to_config(agent));
                map.insert(agent.id.clone(), agent.clone());
            }
            map
        }
        Err(e) => {
            eprintln!("  warning: registry unavailable ({e}); using built-ins only");
            std::collections::HashMap::new()
        }
    };

    state::overlay_install_state_sync(&mut known, &paths.agent_state_file);
    let records: std::collections::HashMap<String, state::InstallRecord> =
        state::load_state_sync(&paths.agent_state_file)
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();

    if known.is_empty() {
        println!("  (no agents configured)");
        return Ok(());
    }

    let discovered = bitrouter_providers::acp::discovery::discover_agents(&known);

    let mut names: Vec<_> = known.keys().cloned().collect();
    names.sort();

    println!();
    println!("  Agents");
    println!("  ──────");
    println!();

    for name in &names {
        let on_path = discovered.iter().any(|d| &d.name == name);
        let installed = records.get(name);
        let status = match (installed, on_path) {
            (Some(rec), _) => format!("\u{2713} installed ({})", rec.method),
            (None, true) => "\u{2713} on PATH".to_owned(),
            (None, false) => "\u{2717} not installed".to_owned(),
        };
        let version = registry_map
            .get(name)
            .map(|r| r.version.as_str())
            .unwrap_or("-");
        println!("  {name:20}  {version:10}  {status}");
    }
    println!();

    Ok(())
}

/// Install an agent by id via the ACP registry.
pub async fn run_install(
    agent_id: &str,
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let cache_file = paths.cache_dir.join("acp-registry.json");
    let registry_url = registry::resolve_registry_url(config.acp_registry_url.as_deref());

    let index = registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &registry_url)
        .await
        .map_err(|e| format!("registry unavailable: {e}"))?;

    let registry_agent = index
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .ok_or_else(|| format!("agent '{agent_id}' not found in registry"))?;

    let agent_config = registry_agent_to_config(registry_agent);
    let install_dir = paths.agent_install_dir(agent_id);
    let (progress_tx, mut progress_rx) = mpsc::channel(32);

    // Pipe progress to stdout.
    let id_copy = agent_id.to_owned();
    let reporter = tokio::spawn(async move {
        while let Some(p) = progress_rx.recv().await {
            match p {
                InstallProgress::Downloading {
                    bytes_received,
                    total,
                } => {
                    if let Some(t) = total {
                        let pct = (t > 0)
                            .then(|| bytes_received.saturating_mul(100).checked_div(t))
                            .flatten()
                            .unwrap_or(0);
                        println!("  [{id_copy}] downloading: {pct}%");
                    } else {
                        println!("  [{id_copy}] downloading...");
                    }
                }
                InstallProgress::Extracting => println!("  [{id_copy}] extracting..."),
                InstallProgress::Done(path) => println!("  [{id_copy}] done: {}", path.display()),
                InstallProgress::Failed(msg) => eprintln!("  [{id_copy}] failed: {msg}"),
            }
        }
    });

    let result = eager::install_agent(
        agent_id,
        &agent_config,
        &install_dir,
        &paths.agent_state_file,
        &registry_agent.version,
        progress_tx,
    )
    .await;

    // Drop side: closing progress_tx is implicit via the move; the
    // reporter task will exit when the channel is empty+closed.
    let _ = reporter.await;

    let installed = result.map_err(|e| format!("install failed: {e}"))?;
    println!();
    println!(
        "  \u{2713} {} installed via {}",
        installed.agent_id, installed.method
    );
    Ok(())
}

/// Uninstall an agent by id.
pub async fn run_uninstall(
    agent_id: &str,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let install_dir = paths.agent_install_dir(agent_id);
    eager::uninstall_agent(agent_id, &install_dir, &paths.agent_state_file)
        .await
        .map_err(|e| format!("uninstall failed: {e}"))?;
    println!("  \u{2713} {agent_id} uninstalled");
    Ok(())
}

/// Update one agent (or all installed agents if `agent_id` is `None`).
pub async fn run_update(
    agent_id: Option<&str>,
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = state::load_state_sync(&paths.agent_state_file);
    if records.is_empty() {
        println!("  (no agents installed)");
        return Ok(());
    }

    let targets: Vec<&str> = match agent_id {
        Some(id) => {
            if !records.iter().any(|r| r.id == id) {
                return Err(format!("agent '{id}' is not installed").into());
            }
            vec![id]
        }
        None => records.iter().map(|r| r.id.as_str()).collect(),
    };

    for id in targets {
        println!("  → updating {id}");
        run_install(id, config, paths).await?;
    }
    Ok(())
}

/// Run the `agents check` subcommand — verify that agent routing through
/// BitRouter is properly configured and working.
///
/// Checks three things:
/// 1. Routing env vars are set in the current shell
/// 2. BitRouter is reachable at the configured listen address
/// 3. Agents are discovered on PATH or distributable
pub fn run_check(config: &BitrouterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let listen = config.server.listen;
    let base = format!("http://{listen}");

    println!();
    println!("  Agent Routing Check");
    println!("  ───────────────────");
    println!();

    // 1. Check env vars
    let env_checks = [
        ("OPENAI_BASE_URL", format!("{base}/v1")),
        ("ANTHROPIC_BASE_URL", format!("{base}/v1")),
        ("GOOGLE_AI_BASE_URL", format!("{base}/v1beta")),
    ];

    let mut env_ok = true;
    for (var, expected) in &env_checks {
        match std::env::var(var) {
            Ok(val) if val == *expected => {
                println!("  \u{2713} {var} = {val}");
            }
            Ok(val) => {
                println!("  \u{26a0} {var} = {val}  (expected {expected})");
                env_ok = false;
            }
            Err(_) => {
                println!("  \u{2717} {var} not set  (expected {expected})");
                env_ok = false;
            }
        }
    }

    if !env_ok {
        println!();
        println!("  Env vars missing or mismatched. To fix, add to your shell profile:");
        for (var, expected) in &env_checks {
            println!("    export {var}={expected}");
        }
        println!();
        println!("  Then run: source ~/.zshrc  (or open a new terminal)");
    }

    // 2. Check BitRouter is reachable (TCP connect probe)
    println!();
    match std::net::TcpStream::connect_timeout(&listen, std::time::Duration::from_secs(2)) {
        Ok(_) => {
            println!("  \u{2713} BitRouter reachable at {listen}");
        }
        Err(_) => {
            println!("  \u{2717} BitRouter not reachable at {listen}");
            println!("    Start the server: bitrouter serve");
        }
    }

    // 3. Check agent discovery
    println!();
    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let discovered = bitrouter_providers::acp::discovery::discover_agents(&known);

    if discovered.is_empty() {
        println!("  \u{2717} No ACP agents discovered");
    } else {
        use bitrouter_providers::acp::types::AgentAvailability;

        let on_path: Vec<&str> = discovered
            .iter()
            .filter(|a| matches!(a.availability, AgentAvailability::OnPath(_)))
            .map(|a| a.name.as_str())
            .collect();
        let distributable: Vec<&str> = discovered
            .iter()
            .filter(|a| matches!(a.availability, AgentAvailability::Distributable))
            .map(|a| a.name.as_str())
            .collect();

        if !on_path.is_empty() {
            println!("  \u{2713} Agents on PATH: {}", on_path.join(", "));
        }
        if !distributable.is_empty() {
            println!(
                "  \u{2713} Agents available (auto-install): {}",
                distributable.join(", ")
            );
        }
    }

    println!();

    Ok(())
}
