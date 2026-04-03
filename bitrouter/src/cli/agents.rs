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
