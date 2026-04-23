//! Slash-command handling for the TUI input bar.
//!
//! When the user types a line starting with `/`, we try to parse it as
//! one of the supported commands and render its output into the active
//! tab's scrollback as system messages.  Unrecognized `/...` input falls
//! back to being sent to the agent as a prompt so users can still talk
//! about slashes.

use bitrouter_config::BitrouterConfig;
use bitrouter_config::acp::registry_agent_to_config;
use bitrouter_providers::acp::types::InstallProgress;
use tokio::sync::mpsc;

use crate::event::AppEvent;

use super::App;

/// Structured form of a recognised slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlashCommand {
    AgentsList { refresh: bool },
    AgentsInstall { id: String },
    AgentsUninstall { id: String },
    AgentsUpdate { id: Option<String> },
    ProvidersList,
    ProvidersUse { mode: String },
    Login,
    Logout,
    Whoami,
    Usage,
    KeysList,
    Init,
    Help,
}

/// Parse a `/`-prefixed input line.
///
/// Returns `None` if the line is not a slash command or is unrecognized
/// (the caller then decides whether to fall through to agent routing).
pub(super) fn parse_slash(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let head = parts.next()?;

    match head {
        "agents" => match parts.next() {
            None | Some("list") => {
                let refresh = parts.any(|p| p == "--refresh");
                Some(SlashCommand::AgentsList { refresh })
            }
            Some("install") => parts
                .next()
                .map(|id| SlashCommand::AgentsInstall { id: id.to_owned() }),
            Some("uninstall") => parts
                .next()
                .map(|id| SlashCommand::AgentsUninstall { id: id.to_owned() }),
            Some("update") => Some(SlashCommand::AgentsUpdate {
                id: parts.next().map(str::to_owned),
            }),
            _ => None,
        },
        "providers" => match parts.next() {
            None | Some("list") => Some(SlashCommand::ProvidersList),
            Some("use") => parts
                .next()
                .map(|m| SlashCommand::ProvidersUse { mode: m.to_owned() }),
            _ => None,
        },
        "login" => Some(SlashCommand::Login),
        "logout" => Some(SlashCommand::Logout),
        "whoami" => Some(SlashCommand::Whoami),
        "usage" => Some(SlashCommand::Usage),
        "keys" => Some(SlashCommand::KeysList),
        "init" => Some(SlashCommand::Init),
        "help" | "?" => Some(SlashCommand::Help),
        _ => None,
    }
}

impl App {
    /// Handle a parsed slash command.  Returns `true` when the input
    /// was consumed (so the caller skips the agent-routing path).
    pub(super) fn try_handle_slash(
        &mut self,
        line: &str,
        bitrouter_config: &BitrouterConfig,
    ) -> bool {
        let Some(cmd) = parse_slash(line) else {
            return false;
        };

        match cmd {
            SlashCommand::AgentsList { refresh } => self.slash_agents_list(refresh),
            SlashCommand::AgentsInstall { id } => self.slash_agents_install(id, bitrouter_config),
            SlashCommand::AgentsUninstall { id } => self.slash_agents_uninstall(id),
            SlashCommand::AgentsUpdate { id } => self.slash_agents_update(id, bitrouter_config),
            SlashCommand::ProvidersList => self.slash_providers_list(bitrouter_config),
            SlashCommand::ProvidersUse { mode } => self.slash_providers_use(mode),
            SlashCommand::Login => self.push_system_msg(
                "Device-flow login runs in the CLI. Exit the TUI (Ctrl+Q) and run: bitrouter login",
            ),
            SlashCommand::Logout => {
                self.push_system_msg("Exit the TUI (Ctrl+Q) and run: bitrouter logout")
            }
            SlashCommand::Whoami => {
                self.push_system_msg("Exit the TUI (Ctrl+Q) and run: bitrouter whoami")
            }
            SlashCommand::Usage => self.push_system_msg(
                "Usage dashboard is not yet available in-TUI; tracked separately.",
            ),
            SlashCommand::KeysList => self.push_system_msg(
                "API keys are managed via `bitrouter auth set/remove` and ~/.bitrouter/.env",
            ),
            SlashCommand::Init => {
                self.push_system_msg("Re-run setup: exit the TUI (Ctrl+Q) and run `bitrouter init`")
            }
            SlashCommand::Help => {
                self.state.modal = Some(crate::model::Modal::Help);
            }
        }
        true
    }

    fn slash_agents_list(&mut self, refresh: bool) {
        let cache_file = self.state.config.cache_dir.join("acp-registry.json");
        let state_file = self.state.config.agent_state_file.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::registry;
            use bitrouter_providers::acp::state;

            let url = registry::resolve_registry_url(None);
            let fetch = if refresh {
                registry::fetch_registry_fresh(&cache_file, &url).await
            } else {
                registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &url).await
            };

            let mut lines: Vec<String> = Vec::new();
            let records = state::load_state_sync(&state_file);

            match fetch {
                Ok(index) => {
                    lines.push(format!(
                        "Agents ({} in registry v{}):",
                        index.agents.len(),
                        index.version
                    ));
                    let mut agents = index.agents.clone();
                    agents.sort_by(|a, b| a.id.cmp(&b.id));
                    for a in &agents {
                        let installed = records.iter().find(|r| r.id == a.id);
                        let mark = match installed {
                            Some(r) => format!("[{}]", method_label(r.method)),
                            None => "[ ]".to_owned(),
                        };
                        lines.push(format!("  {mark} {:<22} v{}", a.id, a.version));
                    }
                }
                Err(e) => lines.push(format!("Registry unavailable: {e}")),
            }

            send_system_lines(&event_tx, lines).await;
        });
    }

    fn slash_agents_install(&mut self, id: String, bitrouter_config: &BitrouterConfig) {
        self.push_system_msg(&format!("Installing {id}..."));

        let cache_file = self.state.config.cache_dir.join("acp-registry.json");
        let state_file = self.state.config.agent_state_file.clone();
        let install_dir = self.state.config.agents_dir.join(&id);
        let registry_override = bitrouter_config.acp_registry_url.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::{eager, registry};

            let url = registry::resolve_registry_url(registry_override.as_deref());
            let index =
                match registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &url).await
                {
                    Ok(i) => i,
                    Err(e) => {
                        send_system_lines(&event_tx, vec![format!("Registry unavailable: {e}")])
                            .await;
                        return;
                    }
                };

            let Some(agent) = index.agents.iter().find(|a| a.id == id) else {
                send_system_lines(
                    &event_tx,
                    vec![format!("Agent '{id}' not found in registry")],
                )
                .await;
                return;
            };

            let agent_config = registry_agent_to_config(agent);
            let (tx, mut rx) = mpsc::channel(32);

            let fwd_id = id.clone();
            let fwd_tx = event_tx.clone();
            let reporter = tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let evt = match p {
                        InstallProgress::Downloading { .. } => AppEvent::InstallProgress {
                            agent_id: fwd_id.clone(),
                            percent: 0,
                        },
                        InstallProgress::Extracting => AppEvent::InstallProgress {
                            agent_id: fwd_id.clone(),
                            percent: 95,
                        },
                        InstallProgress::Done(path) => AppEvent::InstallComplete {
                            agent_id: fwd_id.clone(),
                            binary_path: path,
                        },
                        InstallProgress::Failed(msg) => AppEvent::InstallFailed {
                            agent_id: fwd_id.clone(),
                            message: msg,
                        },
                    };
                    if fwd_tx.send(evt).await.is_err() {
                        break;
                    }
                }
            });

            let result = eager::install_agent(
                &id,
                &agent_config,
                &install_dir,
                &state_file,
                &agent.version,
                tx,
            )
            .await;
            let _ = reporter.await;

            let line = match result {
                Ok(installed) => {
                    format!(
                        "✓ {} installed via {}",
                        installed.agent_id,
                        method_label(installed.method)
                    )
                }
                Err(e) => format!("✗ install failed: {e}"),
            };
            send_system_lines(&event_tx, vec![line]).await;
        });
    }

    fn slash_agents_uninstall(&mut self, id: String) {
        let install_dir = self.state.config.agents_dir.join(&id);
        let state_file = self.state.config.agent_state_file.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::eager;
            let line = match eager::uninstall_agent(&id, &install_dir, &state_file).await {
                Ok(()) => format!("✓ {id} uninstalled"),
                Err(e) => format!("✗ uninstall failed: {e}"),
            };
            send_system_lines(&event_tx, vec![line]).await;
        });
    }

    fn slash_agents_update(&mut self, id: Option<String>, bitrouter_config: &BitrouterConfig) {
        let state_file = self.state.config.agent_state_file.clone();
        let records = bitrouter_providers::acp::state::load_state_sync(&state_file);
        if records.is_empty() {
            self.push_system_msg("(no agents installed)");
            return;
        }
        let targets: Vec<String> = match &id {
            Some(target) => {
                if !records.iter().any(|r| &r.id == target) {
                    self.push_system_msg(&format!("agent '{target}' is not installed"));
                    return;
                }
                vec![target.clone()]
            }
            None => records.iter().map(|r| r.id.clone()).collect(),
        };
        self.push_system_msg(&format!("Updating {} agent(s)...", targets.len()));
        for id in targets {
            self.slash_agents_install(id, bitrouter_config);
        }
    }

    fn slash_providers_list(&mut self, bitrouter_config: &BitrouterConfig) {
        if bitrouter_config.providers.is_empty() {
            self.push_system_msg("(no providers configured)");
            return;
        }
        let mut names: Vec<&String> = bitrouter_config.providers.keys().collect();
        names.sort();
        let mut lines = vec!["Providers:".to_owned()];
        for name in names {
            let p = &bitrouter_config.providers[name];
            let base = p.api_base.as_deref().unwrap_or("(derives)");
            let key = if p.api_key.is_some() {
                "✓ key set"
            } else if p.auth.is_some() {
                "✓ OAuth"
            } else {
                "✗ no creds"
            };
            lines.push(format!("  {name:<20} {base:<40} {key}"));
        }
        for line in lines {
            self.push_system_msg(&line);
        }
    }

    fn slash_providers_use(&mut self, mode: String) {
        let mode = mode.trim().to_lowercase();
        match mode.as_str() {
            "default" | "byok" => self.push_system_msg(&format!(
                "To switch to '{mode}', exit the TUI (Ctrl+Q) and run `bitrouter init`"
            )),
            other => self.push_system_msg(&format!(
                "unknown mode '{other}' (expected 'default' or 'byok')"
            )),
        }
    }
}

fn method_label(method: bitrouter_providers::acp::state::InstallMethod) -> &'static str {
    use bitrouter_providers::acp::state::InstallMethod;
    match method {
        InstallMethod::Npx => "npx",
        InstallMethod::Uvx => "uvx",
        InstallMethod::Binary => "binary",
    }
}

async fn send_system_lines(tx: &mpsc::Sender<AppEvent>, lines: Vec<String>) {
    for line in lines {
        if tx
            .send(AppEvent::SystemMessage { text: line })
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agents_list() {
        assert_eq!(
            parse_slash("/agents"),
            Some(SlashCommand::AgentsList { refresh: false })
        );
        assert_eq!(
            parse_slash("/agents list"),
            Some(SlashCommand::AgentsList { refresh: false })
        );
        assert_eq!(
            parse_slash("/agents list --refresh"),
            Some(SlashCommand::AgentsList { refresh: true })
        );
    }

    #[test]
    fn parses_agents_install_and_uninstall() {
        assert_eq!(
            parse_slash("/agents install claude-acp"),
            Some(SlashCommand::AgentsInstall {
                id: "claude-acp".to_owned()
            })
        );
        assert_eq!(
            parse_slash("/agents uninstall cline"),
            Some(SlashCommand::AgentsUninstall {
                id: "cline".to_owned()
            })
        );
        // Missing id → unrecognised.
        assert_eq!(parse_slash("/agents install"), None);
    }

    #[test]
    fn parses_agents_update_all_or_one() {
        assert_eq!(
            parse_slash("/agents update"),
            Some(SlashCommand::AgentsUpdate { id: None })
        );
        assert_eq!(
            parse_slash("/agents update codex-acp"),
            Some(SlashCommand::AgentsUpdate {
                id: Some("codex-acp".to_owned())
            })
        );
    }

    #[test]
    fn parses_providers_and_misc() {
        assert_eq!(parse_slash("/providers"), Some(SlashCommand::ProvidersList));
        assert_eq!(
            parse_slash("/providers use default"),
            Some(SlashCommand::ProvidersUse {
                mode: "default".to_owned()
            })
        );
        assert_eq!(parse_slash("/login"), Some(SlashCommand::Login));
        assert_eq!(parse_slash("/whoami"), Some(SlashCommand::Whoami));
        assert_eq!(parse_slash("/?"), Some(SlashCommand::Help));
    }

    #[test]
    fn non_slash_or_unknown_returns_none() {
        assert_eq!(parse_slash("hello world"), None);
        assert_eq!(parse_slash("/bogus"), None);
        assert_eq!(parse_slash(""), None);
    }
}
