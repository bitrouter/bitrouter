//! `bitrouter agents` subcommand — list configured and discovered ACP agents.

use bitrouter_config::BitrouterConfig;

/// Run the `agents list` subcommand — prints all agents with PATH availability.
pub fn run_list(config: &BitrouterConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Merge user-configured agents with built-in definitions.
    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    if known.is_empty() {
        println!("  (no agents configured)");
        return Ok(());
    }

    let discovered = bitrouter_providers::acp::discovery::discover_agents(&known);

    let mut names: Vec<_> = known.keys().cloned().collect();
    names.sort();

    for name in &names {
        let agent = &known[name];
        let on_path = discovered.iter().any(|d| &d.name == name);
        let status = if on_path {
            "\u{2713} found"
        } else {
            "\u{2717} not on PATH"
        };
        println!("  {name}  {}  {}  {status}", agent.protocol, agent.binary,);
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
