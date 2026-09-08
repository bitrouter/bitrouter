//! Assembly of the Code surface's operational ports and session launcher.
//!
//! The interactive loop receives this boundary; configuration and target
//! selection remain here, outside the conversation's state machine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_mcp::actions::route::RouteInput;
use bitrouter_tui::machine::PromptCommand;
use tokio_util::sync::CancellationToken;

use crate::acp_cli::{
    SessionHandle, SessionHost, SessionSelection, SpawnContext, lifecycle_cancelled,
};
use crate::contexts::RemoteContext;
use crate::dashboard::SessionRequest;
use crate::output::{CliReport, Format, Output};
use crate::paths::ConfigSource;

/// A selectable ACP facet, with its exact configured/catalog identity.
pub(crate) struct AgentChoice {
    pub id: String,
    pub description: String,
}

enum Target {
    Local {
        source: ConfigSource,
        socket: PathBuf,
    },
    Remote {
        client: crate::remote_control::HttpControlClient,
    },
}

/// The exact launch inputs a legacy interactive entry already resolved.
///
/// The first Code start consumes these rather than reloading config or
/// rebuilding [`crate::acp_cli::LaunchOptions`]. Code then applies its own
/// terminal-auth capability boundary. Later starts are selected by the user
/// inside Code and follow the usual configuration path.
struct InitialLaunch {
    agent_id: String,
    selection: SessionSelection,
    routing: crate::acp_cli::RoutingOptions,
    config: bitrouter_sdk::config::Config,
    options: crate::acp_cli::LaunchOptions,
}

impl InitialLaunch {
    fn matches(&self, request: &SessionRequest) -> bool {
        self.agent_id == request.agent
            && self.selection == request.selection
            && self.routing == request.routing
    }
}

/// Immutable ports shared by asynchronous interactive effects.
pub(crate) struct CodeServices {
    target: Target,
    pub label: String,
    pub operations_only: bool,
    initial_launch: Mutex<Option<InitialLaunch>>,
}

/// Prepared session plus presentation-safe diagnostics and prompt templates.
pub(crate) struct OpenedSession {
    pub handle: SessionHandle,
    pub diagnostics: Vec<String>,
    pub prompt_commands: Vec<PromptCommand>,
    pub resumed: bool,
}

impl CodeServices {
    pub fn commands(
        &self,
        client: Option<&bitrouter_sdk::acp::client::AcpClient>,
    ) -> Vec<bitrouter_tui::machine::Command> {
        if let Some(client) = client {
            return super::session::offered_commands(client);
        }
        bitrouter_mcp::actions::ACTIONS
            .iter()
            .filter_map(|action| {
                if self.operations_only && !operation_command_supported(action.id) {
                    return None;
                }
                let name = action.tui_command?;
                let unavailable =
                    (!matches!(action.requires, bitrouter_mcp::actions::Requires::Nothing))
                        .then_some("Connect a routable ACP session to use this action");
                Some(bitrouter_tui::machine::Command {
                    name,
                    action: action.id,
                    summary: super::session::summary_for(action.id),
                    unavailable,
                })
            })
            .collect()
    }

    pub async fn open(
        remote: Option<(String, RemoteContext)>,
        config: Option<&Path>,
        socket: Option<&Path>,
    ) -> Result<Arc<Self>> {
        let operations_only = remote.is_some() || socket.is_some();
        let (target, label) = match remote {
            Some((name, context)) => {
                ensure!(
                    config.is_none() && socket.is_none(),
                    "remote `code` does not accept --config or --socket; the named context is the complete target"
                );
                (
                    Target::Remote {
                        client: context.client()?,
                    },
                    format!("Remote operations · read-only · {name}"),
                )
            }
            None => {
                let source = crate::paths::resolve_config(config)?;
                let socket = match socket {
                    Some(path) => path.to_path_buf(),
                    None => crate::daemon::socket_path_for(
                        &source,
                        &crate::paths::load_config(&source).await?,
                    ),
                };
                let label = if operations_only {
                    format!("Local operations · read-only · {}", socket.display())
                } else {
                    format!("bitrouter code · {}", std::env::current_dir()?.display())
                };
                (Target::Local { source, socket }, label)
            }
        };
        Ok(Arc::new(Self {
            target,
            label,
            operations_only,
            initial_launch: Mutex::new(None),
        }))
    }

    /// Build Code services from an already-resolved legacy interactive launch.
    ///
    /// This preserves its loaded config and launch inputs for the first
    /// session, while Code keeps ownership of its terminal-auth boundary and
    /// asynchronous conversation lifecycle.
    pub(crate) fn from_spawn_context(ctx: SpawnContext<'_>) -> Result<(Arc<Self>, SessionRequest)> {
        let SpawnContext {
            source,
            config,
            agent_id,
            options,
            routing,
        } = ctx;
        let request = SessionRequest {
            agent: agent_id.to_string(),
            selection: SessionSelection::New,
            turn_timeout: options.turn_timeout.map(|timeout| timeout.as_secs()),
            routing: routing.clone(),
        };
        let source = source.clone();
        let socket = crate::daemon::socket_path_for(&source, &config);
        let label = format!("bitrouter code · {}", std::env::current_dir()?.display());
        let initial_launch = InitialLaunch {
            agent_id: agent_id.to_string(),
            selection: SessionSelection::New,
            routing,
            config,
            options,
        };
        Ok((
            Arc::new(Self {
                target: Target::Local { source, socket },
                label,
                operations_only: false,
                initial_launch: Mutex::new(Some(initial_launch)),
            }),
            request,
        ))
    }

    pub async fn agents(&self) -> Result<Vec<AgentChoice>> {
        if self.operations_only {
            return Ok(Vec::new());
        }
        let Target::Local { source, .. } = &self.target else {
            return Ok(Vec::new());
        };
        let config = crate::paths::load_config(source).await?;
        Ok(crate::agents::list(&config)
            .into_iter()
            .filter_map(|row| {
                agent_is_selectable(&row).then_some(AgentChoice {
                    id: row.id,
                    description: row.description,
                })
            })
            .collect())
    }

    /// Start a local ACP session unless the interactive lifecycle owner has
    /// cancelled it. A cancellation before the session handle is returned
    /// never consumes a controller without reaping it in `SessionHost`.
    pub(crate) async fn start_with_cancel(
        &self,
        request: SessionRequest,
        cancel: &CancellationToken,
    ) -> Result<OpenedSession> {
        if cancel.is_cancelled() {
            return Err(lifecycle_cancelled());
        }
        ensure!(
            !self.operations_only,
            "This target offers read-only operations; ACP execution is unavailable"
        );
        let Target::Local { source, .. } = &self.target else {
            bail!("Remote ACP execution is unavailable");
        };
        let initial = self.take_initial_launch(&request)?;
        let (config, agent_id, options, routing) = match initial {
            Some(initial) => (
                initial.config,
                initial.agent_id,
                initial.options,
                initial.routing,
            ),
            None => {
                let config = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(lifecycle_cancelled()),
                    result = crate::paths::load_config(source) => result?,
                };
                (
                    config,
                    request.agent.clone(),
                    crate::acp_cli::launch_options(request.turn_timeout),
                    request.routing.clone(),
                )
            }
        };
        let prompt_commands = super::session::prompt_commands(&config.chat)?;
        let mut diagnostics = Vec::new();
        let mut record_diagnostic = |message| diagnostics.push(message);
        let host = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(lifecycle_cancelled()),
            result = SessionHost::prepare_with_diagnostics(
                SpawnContext {
                    source,
                    config,
                    agent_id: &agent_id,
                    options,
                    routing,
                },
                false,
                &mut record_diagnostic,
            ) => result?,
        };
        let resumed = matches!(
            request.selection,
            crate::acp_cli::SessionSelection::Resume(_)
        );
        let handle = host
            .open_with_cancel(
                &request.selection,
                std::env::current_dir().context("resolving current directory")?,
                cancel,
            )
            .await?;
        Ok(OpenedSession {
            handle,
            diagnostics,
            prompt_commands,
            resumed,
        })
    }

    fn take_initial_launch(&self, request: &SessionRequest) -> Result<Option<InitialLaunch>> {
        let mut initial = self
            .initial_launch
            .lock()
            .map_err(|_| anyhow::anyhow!("Code initial launch state is unavailable"))?;
        if initial
            .as_ref()
            .is_some_and(|launch| launch.matches(request))
        {
            Ok(initial.take())
        } else {
            Ok(None)
        }
    }

    /// Render the same typed reports as the CLI, only when explicitly requested.
    pub async fn report(&self, action: &str, args: &[String]) -> Result<String> {
        let report: Box<dyn CliReport> = match &self.target {
            Target::Local { source, socket } => {
                if action == "requests" {
                    ensure!(args.is_empty(), "Host requests takes no arguments in Code");
                    Box::new(
                        super::requests::RequestsAction::new(source.clone(), socket.clone())
                            .report(100)
                            .await?,
                    )
                } else {
                    super::session::SessionPorts::open(source.clone(), socket.clone())
                        .run(action, args)
                        .await?
                }
            }
            Target::Remote { client } => match action {
                "status" => {
                    ensure!(args.is_empty(), "usage: /status");
                    Box::new(client.status().await?)
                }
                "list_models" => Box::new(client.models(args.first().map(String::as_str)).await?),
                "requests" => {
                    ensure!(args.is_empty(), "Host requests takes no arguments in Code");
                    Box::new(client.requests(Some(100)).await?)
                }
                "route" => {
                    let model = args.first().context("usage: /preview <model>")?;
                    Box::new(
                        client
                            .route(&RouteInput {
                                model: model.clone(),
                                prompt: None,
                            })
                            .await?,
                    )
                }
                _ => bail!("This remote target does not offer `{action}`"),
            },
        };
        String::from_utf8(Output::new(Format::Human).render_to_vec(report.as_ref()))
            .context("rendering the requested report")
    }
}

/// A socket or named remote target exposes only daemon reports. The typed
/// session controls and route mutations require a local ACP session; route
/// preview remains because it is a read-only report on both operation targets.
fn operation_command_supported(action: &str) -> bool {
    matches!(action, "status" | "list_models" | "route")
}

/// A configured stdio ACP transport is authoritative even when it reuses a
/// catalog id whose bundled harness is interactive-only. Unconfigured catalog
/// entries remain selectable only when the catalog actually supplies ACP.
fn agent_is_selectable(row: &crate::agents::ListRow) -> bool {
    row.configured
        || crate::harness::by_id(&row.id).is_some_and(|harness| harness.acp_command.is_some())
}

#[cfg(test)]
mod tests {
    use super::{CodeServices, agent_is_selectable, operation_command_supported};
    use crate::acp_cli::{LaunchOptions, RoutingOptions, SessionSelection, SpawnContext};
    use crate::dashboard::SessionRequest;
    use crate::paths::ConfigSource;

    fn request(agent: &str) -> SessionRequest {
        SessionRequest {
            agent: agent.to_string(),
            selection: SessionSelection::New,
            turn_timeout: None,
            routing: RoutingOptions::default(),
        }
    }

    #[test]
    fn initial_launch_waits_for_its_matching_agent_request() -> anyhow::Result<()> {
        let directory = std::env::temp_dir().join("bitrouter-code-test");
        let source = ConfigSource::Default { home: directory };
        let (services, initial_request) = CodeServices::from_spawn_context(SpawnContext {
            source: &source,
            config: bitrouter_sdk::config::Config::default(),
            agent_id: "initial",
            options: LaunchOptions {
                strip_inherited_env: vec!["UNRELATED_AGENT_TOKEN".to_string()],
                turn_timeout: Some(std::time::Duration::from_millis(1_501)),
                terminal_auth: true,
                ..LaunchOptions::default()
            },
            routing: RoutingOptions::default(),
        })?;

        assert_eq!(initial_request.agent, "initial");
        assert!(services.take_initial_launch(&request("other"))?.is_none());
        let initial = services
            .take_initial_launch(&request("initial"))?
            .ok_or_else(|| anyhow::anyhow!("matching initial launch was not retained"))?;
        assert_eq!(
            initial.options.strip_inherited_env,
            vec!["UNRELATED_AGENT_TOKEN".to_string()]
        );
        assert_eq!(
            initial.options.turn_timeout,
            Some(std::time::Duration::from_millis(1_501))
        );
        assert!(initial.options.terminal_auth);
        assert!(services.take_initial_launch(&request("initial"))?.is_none());
        Ok(())
    }

    #[test]
    fn operation_targets_offer_only_their_read_only_reports() {
        for action in ["status", "list_models", "route"] {
            assert!(operation_command_supported(action));
        }
        for action in ["commands", "route_set", "route_reset", "skills_search"] {
            assert!(!operation_command_supported(action));
        }
    }

    #[test]
    fn configured_agent_remains_selectable_without_catalog_acp() {
        let configured = crate::agents::ListRow {
            id: "configured-native-name".to_string(),
            configured: true,
            in_catalog: false,
            description: "configured ACP transport".to_string(),
        };
        let unconfigured = crate::agents::ListRow {
            configured: false,
            ..configured.clone()
        };
        assert!(agent_is_selectable(&configured));
        assert!(!agent_is_selectable(&unconfigured));
    }
}
