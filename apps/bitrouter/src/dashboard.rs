//! Application driver for the local/remote operations dashboard.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, PromptResponse, RequestPermissionOutcome,
    SelectedPermissionOutcome, SessionUpdate, TextContent,
};
use anyhow::{Context, Result};
use bitrouter_mcp::actions::models::ModelsReport;
use bitrouter_mcp::actions::route::{ResolvedVia, RouteInput, RouteReport};
use bitrouter_mcp::actions::status::StatusReport;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;

use crate::actions::models::RoutableModels;
use crate::actions::requests::RequestsAction;
use crate::actions::route::RouteAction;
use crate::actions::status::DaemonStatus;
use crate::contexts::RemoteContext;
use crate::output::reports::requests::RequestsReport;
use crate::paths::ConfigSource;

const DASHBOARD_REQUEST_ROWS: u64 = 100;
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Optional initial ACP session selected by `bitrouter code <agent>`.
pub struct SessionRequest {
    pub agent: String,
    pub selection: crate::acp_cli::SessionSelection,
    pub turn_timeout: Option<u64>,
    pub routing: crate::acp_cli::RoutingOptions,
}

struct SessionDriver {
    handle: Option<crate::acp_cli::SessionHandle>,
    updates: std::pin::Pin<Box<dyn futures::Stream<Item = SessionUpdate> + Send>>,
    permissions: std::pin::Pin<
        Box<dyn futures::Stream<Item = bitrouter_sdk::acp::client::PendingPermission> + Send>,
    >,
    pending_permission: Option<bitrouter_sdk::acp::client::PendingPermission>,
    turn: Option<tokio::task::JoinHandle<Result<PromptResponse>>>,
}

impl Default for SessionDriver {
    fn default() -> Self {
        Self {
            handle: None,
            updates: Box::pin(futures::stream::empty()),
            permissions: Box::pin(futures::stream::empty()),
            pending_permission: None,
            turn: None,
        }
    }
}

enum Target {
    Local {
        source: ConfigSource,
        socket: PathBuf,
    },
    Remote {
        name: String,
        client: Box<crate::remote_control::HttpControlClient>,
    },
}

impl Target {
    fn label(&self) -> String {
        match self {
            Self::Local { .. } => "local".to_string(),
            Self::Remote { name, .. } => format!("remote:{name}"),
        }
    }

    async fn snapshot(&self) -> Result<(StatusReport, ModelsReport, RequestsReport)> {
        match self {
            Self::Local { source, socket } => {
                let status_action = DaemonStatus::new(socket, Some(source.clone()));
                let models_action = RoutableModels::new(source.clone(), Some(socket.clone()));
                let requests_action = RequestsAction::new(source.clone(), socket.clone());
                let status = status_action.report();
                let models = models_action.report();
                let requests = requests_action.report(DASHBOARD_REQUEST_ROWS);
                tokio::try_join!(status, models, requests)
            }
            Self::Remote { client, .. } => {
                let status = client.status();
                let models = client.models(None);
                let requests = client.requests(Some(DASHBOARD_REQUEST_ROWS));
                tokio::try_join!(status, models, requests)
            }
        }
    }

    async fn route(&self, model: String) -> Result<RouteReport> {
        let input = RouteInput {
            model,
            prompt: None,
        };
        match self {
            Self::Local { source, socket } => {
                RouteAction::new(source.clone(), Some(socket.clone()))
                    .report(input)
                    .await
            }
            Self::Remote { client, .. } => client.route(&input).await,
        }
    }
}

/// Open the operations dashboard for a local or named remote target.
pub async fn run(
    remote: Option<(String, RemoteContext)>,
    config: Option<&Path>,
    socket: Option<&Path>,
    initial_session: Option<SessionRequest>,
) -> Result<()> {
    let target = match remote {
        Some((name, context)) => {
            if config.is_some() || socket.is_some() {
                return Err(bitrouter_sdk::BitrouterError::bad_request(
                    "remote `tui` does not accept --config or --socket; the named context is \
                     the complete target",
                )
                .into());
            }
            Target::Remote {
                name,
                client: Box::new(context.client()?),
            }
        }
        None => {
            let source = crate::paths::resolve_config(config)?;
            let socket = match socket {
                Some(socket) => socket.to_path_buf(),
                None => crate::daemon::socket_path_for(
                    &source,
                    &crate::paths::load_config(&source).await?,
                ),
            };
            Target::Local { source, socket }
        }
    };

    let mut dashboard = bitrouter_tui::dashboard::Dashboard {
        target: target.label(),
        status: "loading".to_string(),
        ..Default::default()
    };
    dashboard.agents = available_agents(&target).await?;
    refresh(&target, &mut dashboard).await;

    let mut session = SessionDriver::default();
    if let Some(request) = initial_session {
        open_session(&target, &mut dashboard, &mut session, request).await?;
    }

    let mut view =
        bitrouter_tui::dashboard::DashboardView::open().context("opening operations dashboard")?;
    if session.handle.is_some() {
        view.set_page(bitrouter_tui::dashboard::Page::Conversation);
    }
    let outcome = drive(&target, &mut dashboard, &mut session, &mut view).await;
    if let Some(turn) = session.turn.take() {
        turn.abort();
    }
    if let Some(permission) = session.pending_permission.take() {
        permission.deny();
    }
    if let Some(handle) = session.handle.as_mut() {
        handle.shutdown().await;
    }
    view.finish();
    outcome
}

async fn drive(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    session: &mut SessionDriver,
    view: &mut bitrouter_tui::dashboard::DashboardView,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut refresh_tick = tokio::time::interval(REFRESH_INTERVAL);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The initial snapshot was fetched before opening the terminal; do not
    // immediately repeat it on Interval's eager first tick.
    refresh_tick.tick().await;
    let mut shutdown = crate::chat::signals::Shutdown::install();
    view.draw(dashboard)
        .context("drawing operations dashboard")?;

    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(());
                };
                let event = event.context("reading dashboard input")?;
                if handle_event(target, dashboard, session, view, event).await? {
                    return Ok(());
                }
                view.draw(dashboard).context("drawing operations dashboard")?;
            }
            update = session.updates.next() => {
                if let Some(update) = update {
                    dashboard.conversation.journal.apply(update);
                    dashboard.conversation.scroll = 0;
                    view.draw(dashboard).context("drawing ACP update")?;
                }
            }
            permission = session.permissions.next() => {
                if let Some(permission) = permission {
                    let prompt = bitrouter_tui::permission::Prompt::new(
                        permission.request_id.clone(),
                        permission.tool_call.fields.title.clone(),
                        permission.tool_call.tool_call_id.0.to_string(),
                        permission.tool_call.fields.kind,
                        permission.options.clone(),
                    );
                    session.pending_permission = Some(permission);
                    dashboard.conversation.permission = Some(prompt);
                    view.set_page(bitrouter_tui::dashboard::Page::Conversation);
                    view.draw(dashboard).context("drawing permission prompt")?;
                }
            }
            result = pending_turn(&mut session.turn) => {
                session.turn = None;
                dashboard.conversation.status = match result {
                    Ok(response) => format!("idle · {:?}", response.stop_reason),
                    Err(error) => {
                        dashboard.error = Some(format!("ACP turn failed: {error:#}"));
                        "turn failed".to_string()
                    }
                };
                view.draw(dashboard).context("drawing completed turn")?;
            }
            _ = refresh_tick.tick() => {
                refresh(target, dashboard).await;
                view.draw(dashboard).context("drawing operations dashboard")?;
            }
            _ = shutdown.recv() => return Ok(()),
        }
    }
}

async fn handle_event(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    session: &mut SessionDriver,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    event: Event,
) -> Result<bool> {
    match event {
        Event::Resize(_, _) => {}
        Event::Paste(text) => match view.page() {
            bitrouter_tui::dashboard::Page::Route => dashboard
                .route_input
                .push_str(&text.replace(['\r', '\n'], " ")),
            bitrouter_tui::dashboard::Page::Conversation => {
                let _ = bitrouter_tui::dashboard::step(
                    dashboard,
                    bitrouter_tui::dashboard::Action::Paste(text),
                );
            }
            _ => {}
        },
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            if handle_permission_key(dashboard, session, &key) {
                return Ok(false);
            }
            if view.page() == bitrouter_tui::dashboard::Page::Conversation
                && session.turn.is_some()
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                handle_conversation_key(dashboard, session, key).await?;
                return Ok(false);
            }
            if exit_key(&key, view.page()) {
                return Ok(true);
            }
            if key.code == KeyCode::Tab {
                view.next_page();
                return Ok(false);
            }
            match view.page() {
                bitrouter_tui::dashboard::Page::Route => {
                    handle_route_key(target, dashboard, key).await?;
                }
                bitrouter_tui::dashboard::Page::Agents => {
                    handle_agent_key(target, dashboard, session, view, key).await?;
                }
                bitrouter_tui::dashboard::Page::Conversation => {
                    handle_conversation_key(dashboard, session, key).await?;
                }
                _ => handle_navigation_key(target, dashboard, view, key).await,
            }
        }
        _ => {}
    }
    Ok(false)
}

fn exit_key(key: &KeyEvent, page: bitrouter_tui::dashboard::Page) -> bool {
    (page != bitrouter_tui::dashboard::Page::Conversation && key.code == KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        || (page == bitrouter_tui::dashboard::Page::Conversation
            && key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::CONTROL))
        || (page != bitrouter_tui::dashboard::Page::Route
            && page != bitrouter_tui::dashboard::Page::Conversation
            && key.code == KeyCode::Char('q'))
}

async fn pending_turn(
    turn: &mut Option<tokio::task::JoinHandle<Result<PromptResponse>>>,
) -> Result<PromptResponse> {
    match turn {
        Some(turn) => turn
            .await
            .map_err(|error| anyhow::anyhow!("ACP turn task failed: {error}"))?,
        None => std::future::pending().await,
    }
}

async fn available_agents(target: &Target) -> Result<Vec<bitrouter_tui::dashboard::AgentLine>> {
    let Target::Local { source, .. } = target else {
        return Ok(Vec::new());
    };
    let config = crate::paths::load_config(source).await?;
    Ok(crate::agents::list(&config)
        .into_iter()
        .filter_map(|row| {
            let harness = crate::harness::by_id(&row.id);
            let acp = harness.map_or(row.configured, |harness| harness.acp_command.is_some());
            let native = harness.is_some_and(|harness| harness.interactive_binary.is_some());
            acp.then_some(bitrouter_tui::dashboard::AgentLine {
                id: match row.id.as_str() {
                    "claude-acp" => "claude".to_string(),
                    "codex-acp" => "codex".to_string(),
                    _ => row.id,
                },
                native,
                acp,
                configured: row.configured,
                description: row.description,
            })
        })
        .collect())
}

async fn open_session(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    request: SessionRequest,
) -> Result<()> {
    let Target::Local { source, .. } = target else {
        return Err(bitrouter_sdk::BitrouterError::bad_request(
            "remote ACP conversations are not available; use the operations views",
        )
        .into());
    };
    if let Some(turn) = driver.turn.take() {
        turn.abort();
    }
    if let Some(permission) = driver.pending_permission.take() {
        permission.deny();
    }
    if let Some(handle) = driver.handle.as_mut() {
        handle.shutdown().await;
    }
    let config = crate::paths::load_config(source).await?;
    let context = crate::acp_cli::SpawnContext {
        source,
        config,
        agent_id: &request.agent,
        options: crate::acp_cli::launch_options(request.turn_timeout),
        routing: request.routing,
    };
    // The full-screen shell cannot relinquish the terminal for an external
    // authentication flow, so advertise only the capabilities it can honor.
    let host = crate::acp_cli::SessionHost::prepare(context, false).await?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mut handle = host.open(&request.selection, cwd).await?;
    driver.updates = handle.take_updates();
    driver.permissions = handle.take_permissions();
    dashboard.conversation = bitrouter_tui::dashboard::Conversation {
        agent: Some(handle.agent_id.clone()),
        native_session_id: Some(handle.session_id.clone()),
        provider_session_id: handle.agent_session_id.clone(),
        lifecycle: Some(handle.capabilities.lifecycle_summary()),
        route: handle.via.clone(),
        status: "idle".to_string(),
        ..Default::default()
    };
    driver.handle = Some(handle);
    dashboard.error = None;
    Ok(())
}

fn handle_permission_key(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    key: &KeyEvent,
) -> bool {
    let Some(prompt) = dashboard.conversation.permission.as_ref() else {
        return false;
    };
    // Ctrl-C belongs to the turn, even while a permission picker is open.
    // Deny the parked request first, then let the conversation handler cancel
    // the turn so cancellation can never be interpreted as consent.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(permission) = driver.pending_permission.take() {
            permission.deny();
        }
        dashboard.conversation.permission = None;
        return false;
    }
    let outcome = match key.code {
        KeyCode::Char(character) => prompt.choose(character).map(|option_id| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id))
        }),
        KeyCode::Esc => Some(prompt.unanswered()),
        _ => None,
    };
    let Some(outcome) = outcome else {
        return true;
    };
    if let Some(permission) = driver.pending_permission.take() {
        permission.resolve(outcome);
    }
    dashboard.conversation.permission = None;
    true
}

async fn handle_agent_key(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Up => {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::SelectPreviousAgent,
            );
        }
        KeyCode::Down => {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::SelectNextAgent,
            );
        }
        KeyCode::Enter => {
            let Some(bitrouter_tui::dashboard::Effect::StartAgent(agent)) =
                bitrouter_tui::dashboard::step(
                    dashboard,
                    bitrouter_tui::dashboard::Action::ActivateSelectedAgent,
                )
            else {
                dashboard.error =
                    Some("No local ACP agents are available for this target.".to_string());
                return Ok(());
            };
            let request = SessionRequest {
                agent,
                selection: crate::acp_cli::SessionSelection::New,
                turn_timeout: None,
                routing: crate::acp_cli::RoutingOptions::default(),
            };
            match open_session(target, dashboard, driver, request).await {
                Ok(()) => view.set_page(bitrouter_tui::dashboard::Page::Conversation),
                Err(error) => dashboard.error = Some(format!("Could not start agent: {error:#}")),
            }
        }
        _ => handle_navigation_key(target, dashboard, view, key).await,
    }
    Ok(())
}

async fn handle_conversation_key(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::PageUp => {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::ScrollUp,
            );
        }
        KeyCode::PageDown => {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::ScrollDown,
            );
        }
        KeyCode::Backspace if driver.turn.is_none() => {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::Backspace,
            );
        }
        KeyCode::Enter if driver.turn.is_none() => {
            let Some(bitrouter_tui::dashboard::Effect::Prompt(prompt)) =
                bitrouter_tui::dashboard::step(
                    dashboard,
                    bitrouter_tui::dashboard::Action::SubmitPrompt,
                )
            else {
                return Ok(());
            };
            let Some(handle) = driver.handle.as_ref() else {
                dashboard.error = Some("Select an ACP agent before sending a prompt.".to_string());
                return Ok(());
            };
            dashboard
                .conversation
                .journal
                .apply(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(prompt.clone())),
                )));
            dashboard.conversation.scroll = 0;
            dashboard.conversation.status = "working".to_string();
            let client = handle.client.clone();
            let session_id = handle.session_id.clone();
            driver.turn = Some(tokio::spawn(async move {
                client.prompt(&session_id, &prompt).await
            }));
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let (Some(handle), Some(turn)) = (driver.handle.as_ref(), driver.turn.take()) {
                turn.abort();
                handle.client.deny_session_permissions(&handle.session_id);
                handle.client.cancel(&handle.session_id).await?;
                dashboard.conversation.status = "cancelled".to_string();
            }
        }
        KeyCode::Esc => {}
        KeyCode::Char(character)
            if driver.turn.is_none()
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            let _ = bitrouter_tui::dashboard::step(
                dashboard,
                bitrouter_tui::dashboard::Action::Type(character),
            );
        }
        _ => {}
    }
    Ok(())
}

async fn handle_navigation_key(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Char('1') => view.set_page(bitrouter_tui::dashboard::Page::Home),
        KeyCode::Char('2') => view.set_page(bitrouter_tui::dashboard::Page::Agents),
        KeyCode::Char('3') => view.set_page(bitrouter_tui::dashboard::Page::Conversation),
        KeyCode::Char('4') => view.set_page(bitrouter_tui::dashboard::Page::Sessions),
        KeyCode::Char('5') => view.set_page(bitrouter_tui::dashboard::Page::Models),
        KeyCode::Char('6') => view.set_page(bitrouter_tui::dashboard::Page::Requests),
        KeyCode::Char('7') => view.set_page(bitrouter_tui::dashboard::Page::Route),
        KeyCode::Char('r') => refresh(target, dashboard).await,
        _ => {}
    }
}

async fn handle_route_key(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Enter if !dashboard.route_input.trim().is_empty() => {
            let model = dashboard.route_input.trim().to_string();
            match target.route(model).await {
                Ok(report) => {
                    dashboard.route = Some(route_line(report));
                    dashboard.error = None;
                }
                Err(error) => dashboard.error = Some(error.to_string()),
            }
        }
        KeyCode::Backspace => {
            dashboard.route_input.pop();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            dashboard.route_input.clear();
            dashboard.route = None;
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            dashboard.route_input.push(character);
        }
        _ => {}
    }
    Ok(())
}

async fn refresh(target: &Target, dashboard: &mut bitrouter_tui::dashboard::Dashboard) {
    match target.snapshot().await {
        Ok((status, models, requests)) => {
            let route_input = std::mem::take(&mut dashboard.route_input);
            let route = dashboard.route.take();
            let snapshot = dashboard_from_reports(target.label(), status, models, requests);
            dashboard.target = snapshot.target;
            dashboard.connected = snapshot.connected;
            dashboard.status = snapshot.status;
            dashboard.pid = snapshot.pid;
            dashboard.listen = snapshot.listen;
            dashboard.providers = snapshot.providers;
            dashboard.spend = snapshot.spend;
            dashboard.models = snapshot.models;
            dashboard.requests = snapshot.requests;
            dashboard.route_input = route_input;
            dashboard.route = route;
            dashboard.error = snapshot.error;
        }
        Err(error) => {
            dashboard.connected = false;
            dashboard.status = "unavailable".to_string();
            dashboard.error = Some(error.to_string());
        }
    }
}

fn dashboard_from_reports(
    target: String,
    status: StatusReport,
    models: ModelsReport,
    requests: RequestsReport,
) -> bitrouter_tui::dashboard::Dashboard {
    let model_lines = models
        .models
        .into_iter()
        .map(|model| bitrouter_tui::dashboard::ModelLine {
            id: model.id,
            providers: model.providers.join(", "),
        })
        .collect();
    let request_lines = requests
        .rows
        .iter()
        .map(|request| {
            let [time, model, provider, input, output, cost, _latency, status] =
                request.display_cells();
            bitrouter_tui::dashboard::RequestLine {
                time,
                model,
                provider,
                tokens: format!("{input}/{output}"),
                cost,
                status,
            }
        })
        .collect();
    let spend = status.spend.as_ref().and_then(|spend| {
        spend.spent.as_ref().map(|spent| {
            let amount = crate::metering::fmt_usd(spent.estimated_micro_usd);
            if spent.unpriced == 0 {
                format!("{amount} {} · {} requests", spent.window, spent.requests)
            } else {
                format!(
                    "{amount}+ {} · {} requests · {} unpriced",
                    spent.window, spent.requests, spent.unpriced
                )
            }
        })
    });
    bitrouter_tui::dashboard::Dashboard {
        target,
        connected: status.running,
        status: if status.running {
            "running".to_string()
        } else {
            "stopped".to_string()
        },
        pid: status.pid,
        listen: status.listen,
        providers: status.providers,
        spend,
        models: model_lines,
        requests: request_lines,
        route_input: String::new(),
        route: None,
        error: None,
        ..Default::default()
    }
}

fn route_line(report: RouteReport) -> bitrouter_tui::dashboard::RouteLine {
    let resolved_via = match report.resolved_via {
        ResolvedVia::Live => "live",
        ResolvedVia::Config => "config",
        ResolvedVia::ZeroConfig => "zero config",
    };
    bitrouter_tui::dashboard::RouteLine {
        requested: report.requested_model,
        effective: report.effective_model,
        providers: report
            .provider_chain
            .iter()
            .map(|hop| hop.provider.as_str())
            .collect::<Vec<_>>()
            .join(" → "),
        resolved_via: resolved_via.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_page_keeps_requested_and_effective_models_distinct() {
        let line = route_line(RouteReport {
            requested_model: "cheap".to_string(),
            effective_model: "capable".to_string(),
            effective_effort: None,
            resolved_via: ResolvedVia::Config,
            policy_decision: None,
            provider_chain: Vec::new(),
            estimated_cost: None,
        });
        assert_eq!(line.requested, "cheap");
        assert_eq!(line.effective, "capable");
        assert_eq!(line.resolved_via, "config");
    }
}
