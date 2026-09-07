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

pub(crate) mod tasks;

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
    tasks: tasks::TaskDriver,
}

impl Default for SessionDriver {
    fn default() -> Self {
        Self {
            handle: None,
            updates: Box::pin(futures::stream::empty()),
            permissions: Box::pin(futures::stream::empty()),
            pending_permission: None,
            turn: None,
            tasks: tasks::TaskDriver::default(),
        }
    }
}

impl SessionDriver {
    async fn shutdown(&mut self) {
        self.tasks = tasks::TaskDriver::default();
        if let Some(turn) = self.turn.take() {
            turn.abort();
        }
        if let Some(permission) = self.pending_permission.take() {
            permission.deny();
        }
        self.updates = Box::pin(futures::stream::empty());
        self.permissions = Box::pin(futures::stream::empty());
        // Retire the handle before awaiting teardown. A failed replacement
        // must not leave a closed client or an already-polled join handle.
        if let Some(mut handle) = self.handle.take() {
            handle.shutdown().await;
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

    let mut view = match bitrouter_tui::dashboard::DashboardView::open() {
        Ok(view) => view,
        Err(error) => {
            session.shutdown().await;
            return Err(error).context("opening operations dashboard");
        }
    };
    if session.handle.is_some() {
        view.set_page(bitrouter_tui::dashboard::Page::Conversation);
    }
    let outcome = drive(&target, &mut dashboard, &mut session, &mut view).await;
    session.shutdown().await;
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
            update = session.updates.next(), if session.handle.is_some() => {
                if let Some(update) = update {
                    dashboard.conversation.journal.apply(update);
                    dashboard.conversation.scroll = 0;
                    view.draw(dashboard).context("drawing ACP update")?;
                } else {
                    session.updates = Box::pin(futures::stream::pending());
                }
            }
            permission = session.permissions.next(), if session.handle.is_some() => {
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
                } else {
                    session.permissions = Box::pin(futures::stream::pending());
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
                session.tasks.invalidate();
                refresh_task(dashboard, session);
                view.draw(dashboard).context("drawing completed turn")?;
            }
            result = session.tasks.result() => {
                session.tasks.complete(result);
                publish_task(dashboard, session);
                view.draw(dashboard).context("drawing task state")?;
            }
            _ = refresh_tick.tick() => {
                refresh(target, dashboard).await;
                refresh_task(dashboard, session);
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
    driver.shutdown().await;
    dashboard.conversation.status = "disconnected".to_string();
    dashboard.conversation.permission = None;
    dashboard.conversation.task = None;
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
    let mut diagnostics = Vec::new();
    let prepared =
        crate::acp_cli::SessionHost::prepare_with_diagnostics(context, false, &mut |message| {
            diagnostics.push(message)
        })
        .await;
    dashboard.notice = (!diagnostics.is_empty()).then(|| diagnostics.join("\n"));
    let host = prepared?;
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
    refresh_task(dashboard, driver);
    dashboard.error = None;
    Ok(())
}

fn publish_task(dashboard: &mut bitrouter_tui::dashboard::Dashboard, driver: &SessionDriver) {
    dashboard.conversation.task = driver
        .handle
        .as_ref()
        .and_then(|handle| driver.tasks.view(&handle.client));
}

fn refresh_task(dashboard: &mut bitrouter_tui::dashboard::Dashboard, driver: &mut SessionDriver) {
    if let Some(handle) = &driver.handle {
        driver.tasks.refresh(&handle.client, &handle.session_id);
    }
    publish_task(dashboard, driver);
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
        KeyCode::F(number @ (2 | 3)) if key.kind == KeyEventKind::Press => {
            if let Some(handle) = &driver.handle {
                let mode = if number == 2 {
                    bitrouter_sdk::acp::controller::tasks::TaskSelectionMode::NewTask
                } else {
                    bitrouter_sdk::acp::controller::tasks::TaskSelectionMode::Retry
                };
                dashboard.error = driver
                    .tasks
                    .select(
                        &handle.client,
                        &handle.session_id,
                        mode,
                        driver.turn.is_some(),
                    )
                    .err();
            }
        }
        KeyCode::F(4) if key.kind == KeyEventKind::Press => refresh_task(dashboard, driver),
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
            let Some(handle) = driver.handle.as_ref() else {
                dashboard.error = Some("Select an ACP agent before sending a prompt.".to_string());
                return Ok(());
            };
            if !driver.tasks.can_prompt(&handle.client) {
                dashboard.error = Some(
                    "Confirm the task selection or refresh task state before sending this message."
                        .into(),
                );
                return Ok(());
            }
            let Some(bitrouter_tui::dashboard::Effect::Prompt(prompt)) =
                bitrouter_tui::dashboard::step(
                    dashboard,
                    bitrouter_tui::dashboard::Action::SubmitPrompt,
                )
            else {
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
            dashboard.error = None;
            driver.tasks.invalidate();
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
                driver.tasks.invalidate();
                refresh_task(dashboard, driver);
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
    publish_task(dashboard, driver);
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
            apply_snapshot(dashboard, snapshot);
            dashboard.route_input = route_input;
            dashboard.route = route;
        }
        Err(error) => {
            dashboard.connected = false;
            dashboard.status = "unavailable".to_string();
            dashboard.refresh_error = Some(error.to_string());
        }
    }
}

/// Apply only server-owned snapshot fields. Drafts, action diagnostics, and
/// the active ACP conversation belong to the interactive client and must not
/// be erased by the background polling cadence.
fn apply_snapshot(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    snapshot: bitrouter_tui::dashboard::Dashboard,
) {
    dashboard.target = snapshot.target;
    dashboard.connected = snapshot.connected;
    dashboard.status = snapshot.status;
    dashboard.pid = snapshot.pid;
    dashboard.listen = snapshot.listen;
    dashboard.providers = snapshot.providers;
    dashboard.spend = snapshot.spend;
    dashboard.models = snapshot.models;
    dashboard.requests = snapshot.requests;
    dashboard.refresh_error = None;
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
pub(crate) mod tests {
    use super::*;

    /// Exercise the actual key handler using a live SessionHost fixture. The
    /// caller verifies database persistence and native lifecycle forwarding.
    pub(crate) async fn exercise_task_keys(
        handle: crate::acp_cli::SessionHandle,
    ) -> Result<crate::acp_cli::SessionHandle> {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard::default();
        dashboard.conversation.native_session_id = Some(handle.session_id.clone());
        let mut driver = SessionDriver {
            handle: Some(handle),
            ..Default::default()
        };
        refresh_task(&mut dashboard, &mut driver);
        dashboard.conversation.input = "keep this draft".into();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_conversation_key(&mut dashboard, &mut driver, enter).await?;
        assert_eq!(dashboard.conversation.input, "keep this draft");
        assert!(driver.turn.is_none());
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), driver.tasks.result()).await?;
        driver.tasks.complete(result);
        publish_task(&mut dashboard, &driver);
        for key in [3, 2] {
            let before = dashboard
                .conversation
                .task
                .as_ref()
                .context("task before selection")?
                .clone();
            handle_conversation_key(
                &mut dashboard,
                &mut driver,
                KeyEvent::new(KeyCode::F(key), KeyModifiers::NONE),
            )
            .await?;
            handle_conversation_key(&mut dashboard, &mut driver, enter).await?;
            assert_eq!(dashboard.conversation.input, "keep this draft");
            assert!(driver.turn.is_none());
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), driver.tasks.result())
                    .await?;
            driver.tasks.complete(result);
            publish_task(&mut dashboard, &driver);
            let selected = dashboard
                .conversation
                .task
                .as_ref()
                .context("selected task")?;
            assert_eq!(selected.attempt_id, before.attempt_id);
            assert!(selected.next_prompt.is_some());
            handle_conversation_key(&mut dashboard, &mut driver, enter).await?;
            let turn = driver.turn.take().context("key handler started prompt")?;
            tokio::time::timeout(std::time::Duration::from_secs(10), turn).await???;
            driver.tasks.invalidate();
            refresh_task(&mut dashboard, &mut driver);
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), driver.tasks.result())
                    .await?;
            driver.tasks.complete(result);
            publish_task(&mut dashboard, &driver);
            let after = dashboard
                .conversation
                .task
                .as_ref()
                .context("next attempt")?;
            assert_ne!(after.attempt_id, before.attempt_id);
            assert_eq!(after.task_id == before.task_id, key == 3);
            assert!(after.next_prompt.is_none());
            dashboard.conversation.input = "keep this draft".into();
        }
        driver.handle.take().context("live session")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_replacement_retires_the_old_session_before_retry_or_exit() -> Result<()> {
        use bitrouter_sdk::acp::transport::{AcpAgentConfig, AcpTransport};
        let directory = tempfile::tempdir()?;
        let source = ConfigSource::File(directory.path().join("bitrouter.yaml"));
        let mut config = bitrouter_sdk::config::Config::default();
        config.agents.insert("fixture".into(), AcpAgentConfig {
            name: "fixture".into(), transport: AcpTransport::Stdio {
                command: "bash".into(), args: vec!["-c".into(), r#"
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id";;
    *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"fixture-session"}}\n' "$id";;
    *session/prompt*) printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
  esac
done
"#.into()], env: Default::default(),
            },
        });
        tokio::fs::write(
            directory.path().join("bitrouter.yaml"),
            serde_json::to_vec(&serde_json::json!({"agents":config.agents}))?,
        )
        .await?;
        let target = Target::Local {
            source,
            socket: directory.path().join("unused.sock"),
        };
        let request = |agent: &str| SessionRequest {
            agent: agent.into(),
            selection: crate::acp_cli::SessionSelection::New,
            turn_timeout: Some(5),
            routing: crate::acp_cli::RoutingOptions {
                direct: true,
                ..Default::default()
            },
        };
        let mut dashboard = bitrouter_tui::dashboard::Dashboard::default();
        let mut driver = SessionDriver::default();
        open_session(&target, &mut dashboard, &mut driver, request("fixture")).await?;
        assert!(driver.handle.is_some());
        for _ in 0..2 {
            assert!(
                open_session(
                    &target,
                    &mut dashboard,
                    &mut driver,
                    request("missing-fixture-agent")
                )
                .await
                .is_err()
            );
            assert!(driver.handle.is_none());
            assert!(driver.updates.next().await.is_none());
            assert!(driver.permissions.next().await.is_none());
            assert_eq!(dashboard.conversation.status, "disconnected");
        }
        open_session(&target, &mut dashboard, &mut driver, request("fixture")).await?;
        let handle = driver.handle.as_ref().context("replacement handle")?;
        handle.client.prompt(&handle.session_id, "fixture").await?;
        driver.shutdown().await;
        driver.shutdown().await;
        assert!(driver.handle.is_none());
        Ok(())
    }

    #[test]
    fn snapshot_refresh_preserves_interactive_diagnostics() {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard {
            error: Some("Could not start agent".to_string()),
            refresh_error: Some("connection interrupted".to_string()),
            notice: Some("routing fallback".to_string()),
            ..Default::default()
        };
        let snapshot = bitrouter_tui::dashboard::Dashboard {
            target: "local".to_string(),
            connected: true,
            status: "running".to_string(),
            ..Default::default()
        };

        apply_snapshot(&mut dashboard, snapshot);

        assert_eq!(dashboard.error.as_deref(), Some("Could not start agent"));
        assert_eq!(dashboard.notice.as_deref(), Some("routing fallback"));
        assert_eq!(dashboard.refresh_error, None);
        assert!(dashboard.connected);
    }

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
