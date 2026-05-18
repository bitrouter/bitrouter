//! `bitrouter agents` subcommand — list, install, uninstall, update, and
//! check ACP agents.

use std::io::{self, Write};

use bitrouter::providers::acp::ops::{self, AcpPaths, AgentList, RoutingCheck};
use bitrouter::providers::acp::types::InstallProgress;
use bitrouter_config::BitrouterConfig;

use crate::cli::OutputFormat;
use bitrouter::runtime::paths::RuntimePaths;

fn paths_from_runtime(paths: &RuntimePaths) -> AcpPaths {
    AcpPaths {
        cache_dir: paths.cache_dir.clone(),
        agents_dir: paths.agents_dir.clone(),
        agent_state_file: paths.agent_state_file.clone(),
    }
}

pub fn render_list_text(list: &AgentList, w: &mut impl Write) -> io::Result<()> {
    if list.agents.is_empty() {
        writeln!(w, "  (no agents configured)")?;
        return Ok(());
    }
    for warning in &list.warnings {
        eprintln!("  warning: {warning}");
    }
    eprintln!();
    eprintln!("  Agents");
    eprintln!("  ──────");
    eprintln!();
    for info in &list.agents {
        let status = match (&info.installed, info.on_path) {
            (Some(rec), _) => format!("\u{2713} installed ({})", rec.method),
            (None, true) => "\u{2713} on PATH".to_owned(),
            (None, false) => "\u{2717} not installed".to_owned(),
        };
        let version = info.version.as_deref().unwrap_or("-");
        writeln!(w, "  {:<20}  {:<10}  {status}", info.id, version)?;
    }
    writeln!(w)?;
    Ok(())
}

/// Run the `agents list` subcommand.
pub async fn run_list(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    refresh: bool,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let acp_paths = paths_from_runtime(paths);
    let list = ops::list_agents(config, &acp_paths, refresh).await?;
    match output {
        OutputFormat::Text => render_list_text(&list, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &list)?,
    }
    Ok(())
}

/// Install an agent by id via the ACP registry.
pub async fn run_install(
    agent_id: &str,
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let acp_paths = paths_from_runtime(paths);
    let mut handle = ops::install_agent(agent_id, config, &acp_paths);
    let id = agent_id.to_owned();
    while let Some(p) = handle.progress.recv().await {
        match p {
            InstallProgress::Downloading {
                bytes_received,
                total,
            } => {
                if let Some(t) = total {
                    let pct = bytes_received
                        .saturating_mul(100)
                        .checked_div(t)
                        .unwrap_or(0);
                    eprintln!("  [{id}] downloading: {pct}%");
                } else {
                    eprintln!("  [{id}] downloading...");
                }
            }
            InstallProgress::Extracting => eprintln!("  [{id}] extracting..."),
            InstallProgress::Done(path) => eprintln!("  [{id}] done: {}", path.display()),
            InstallProgress::Failed(msg) => eprintln!("  [{id}] failed: {msg}"),
        }
    }
    let installed = handle.result.await??;
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
    let acp_paths = paths_from_runtime(paths);
    ops::uninstall_agent(agent_id, &acp_paths).await?;
    println!("  \u{2713} {agent_id} uninstalled");
    Ok(())
}

/// Update one agent (or all installed agents if `agent_id` is `None`).
pub async fn run_update(
    agent_id: Option<&str>,
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let acp_paths = paths_from_runtime(paths);
    let records = bitrouter::providers::acp::state::load_state_sync(&paths.agent_state_file);
    if records.is_empty() {
        println!("  (no agents installed)");
        return Ok(());
    }

    let targets: Vec<String> = match agent_id {
        Some(id) => {
            if !records.iter().any(|r| r.id == id) {
                return Err(format!("agent '{id}' is not installed").into());
            }
            vec![id.to_owned()]
        }
        None => records.iter().map(|r| r.id.clone()).collect(),
    };

    for id in &targets {
        eprintln!("  → updating {id}");
        let mut handle = ops::install_agent(id, config, &acp_paths);
        while let Some(p) = handle.progress.recv().await {
            match p {
                InstallProgress::Downloading {
                    bytes_received,
                    total,
                } => {
                    if let Some(t) = total {
                        let pct = bytes_received
                            .saturating_mul(100)
                            .checked_div(t)
                            .unwrap_or(0);
                        eprintln!("  [{id}] downloading: {pct}%");
                    } else {
                        eprintln!("  [{id}] downloading...");
                    }
                }
                InstallProgress::Extracting => eprintln!("  [{id}] extracting..."),
                InstallProgress::Done(path) => eprintln!("  [{id}] done: {}", path.display()),
                InstallProgress::Failed(msg) => eprintln!("  [{id}] failed: {msg}"),
            }
        }
        let installed = handle.result.await??;
        println!(
            "  \u{2713} {} installed via {}",
            installed.agent_id, installed.method
        );
    }
    Ok(())
}

pub fn render_check_text(check: &RoutingCheck, w: &mut impl Write) -> io::Result<()> {
    eprintln!();
    eprintln!("  Agent Routing Check");
    eprintln!("  ───────────────────");
    eprintln!();

    if check.shim_entries.is_empty() {
        eprintln!("  (no agents with a known routing mapping discovered)");
    } else {
        let mut needs_shim: Vec<&str> = Vec::new();
        for entry in &check.shim_entries {
            if entry.shim_installed {
                eprintln!(
                    "  \u{2713} {} shim \u{2192} {}",
                    entry.agent_id,
                    entry.shim_path.display()
                );
            } else {
                eprintln!("  \u{2717} {} shim not installed", entry.agent_id);
                needs_shim.push(&entry.agent_id);
            }
        }
        if !needs_shim.is_empty() {
            eprintln!();
            eprintln!(
                "  Run `bitrouter init` to install shims for: {}",
                needs_shim.join(", ")
            );
        }
    }

    eprintln!();
    if check.server_reachable {
        writeln!(w, "  \u{2713} BitRouter reachable at {}", check.listen_addr)?;
    } else {
        writeln!(
            w,
            "  \u{2717} BitRouter not reachable at {}",
            check.listen_addr
        )?;
        writeln!(w, "    Start the server: bitrouter serve")?;
    }

    eprintln!();
    if check.discovered_on_path.is_empty() && check.discovered_distributable.is_empty() {
        eprintln!("  \u{2717} No ACP agents discovered");
    } else {
        if !check.discovered_on_path.is_empty() {
            eprintln!(
                "  \u{2713} Agents on PATH: {}",
                check.discovered_on_path.join(", ")
            );
        }
        if !check.discovered_distributable.is_empty() {
            eprintln!(
                "  \u{2713} Agents available (auto-install): {}",
                check.discovered_distributable.join(", ")
            );
        }
    }

    eprintln!();
    Ok(())
}

/// Run the `agents check` subcommand.
pub fn run_check(
    config: &BitrouterConfig,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let check = ops::check_routing(config);
    match output {
        OutputFormat::Text => render_check_text(&check, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &check)?,
    }
    Ok(())
}
