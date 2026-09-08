//! Application driver for the local/remote operations dashboard.

use std::path::Path;
use std::time::Duration;

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

use crate::actions::administration::{
    AgentsReport, ObserveReport, PolicyDetail, PolicyInput, PolicyReport, PolicyView,
    ProvidersReport,
};
use crate::actions::requests::RequestFilters;
use crate::administration_target::{ControlAuthority, InspectionTarget, ReloadSubmission};
use crate::contexts::RemoteContext;
use crate::output::reports::requests::RequestsReport;
use crate::reload::{
    ReloadConsistency, ReloadOutcome, ReloadParticipant, ReloadParticipantOutcome, ReloadState,
};
use crate::remote_control::operations::{OperationReport, OperationStatus, ReloadInput};

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

/// One explicit reload action in flight. Refresh timers never create this
/// task: it exists only after Enter or Ctrl-R asks for a reload.
struct ReloadTask {
    updates: tokio::sync::mpsc::UnboundedReceiver<ReloadUpdate>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ReloadTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

enum ReloadUpdate {
    /// A remote request UUID is durable enough to show before the POST
    /// completes, so an interrupted response remains recoverable.
    Prepared {
        request_id: String,
        server_instance_id: String,
        generation_before: u64,
    },
    RemoteSubmitted(OperationReport),
    RemoteFinished(OperationReport),
    LocalFinished(ReloadState),
    Failed(String),
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

/// Open the operations dashboard for a local or named remote target.
pub async fn run(
    remote: Option<(String, RemoteContext)>,
    config: Option<&Path>,
    socket: Option<&Path>,
    initial_session: Option<SessionRequest>,
) -> Result<()> {
    let target = InspectionTarget::resolve(remote, config, socket).await?;

    let mut dashboard = bitrouter_tui::dashboard::Dashboard {
        target: target.label(),
        status: "loading".to_string(),
        ..Default::default()
    };
    dashboard.can_launch_agents = target.allows_agent_launch();
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
    target: &InspectionTarget,
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
    let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = crate::chat::signals::Shutdown::install();
    let mut reload_task = None;
    view.draw(dashboard)
        .context("drawing operations dashboard")?;

    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(());
                };
                let event = event.context("reading dashboard input")?;
                if handle_event(target, dashboard, session, view, &mut reload_task, event).await? {
                    return Ok(());
                }
                view.draw(dashboard).context("drawing operations dashboard")?;
            }
            update = session.updates.next() => {
                if let Some(update) = update {
                    dashboard.conversation.journal.apply(update);
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
            _ = animation_tick.tick(), if session.turn.is_some() && session.pending_permission.is_none() => {
                view.tick();
                view.draw(dashboard).context("drawing thinking indicator")?;
            }
            _ = refresh_tick.tick() => {
                refresh(target, dashboard).await;
                view.draw(dashboard).context("drawing operations dashboard")?;
            }
            update = next_reload_update(&mut reload_task) => {
                match update {
                    Some(update) => {
                        apply_reload_update(dashboard, update);
                        view.draw(dashboard).context("drawing reload action")?;
                    }
                    None => {
                        // The worker owns all ordinary error paths and sends a
                        // typed diagnostic before closing. A closed channel
                        // here simply makes another explicit reload available.
                        reload_task = None;
                    }
                }
            }
            _ = shutdown.recv() => return Ok(()),
        }
    }
}

async fn handle_event(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    session: &mut SessionDriver,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    reload_task: &mut Option<ReloadTask>,
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
            if bitrouter_tui::editor::is_redraw(&Event::Key(key)) {
                view.invalidate();
            }
            if key.code == KeyCode::Tab {
                view.next_page();
                return Ok(false);
            }
            if key.code == KeyCode::Char('r')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && view.page() == bitrouter_tui::dashboard::Page::Reload
            {
                start_reload(target, dashboard, reload_task);
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
                bitrouter_tui::dashboard::Page::Policy => {
                    handle_policy_key(target, dashboard, view, key).await?;
                }
                bitrouter_tui::dashboard::Page::Reload => {
                    handle_reload_key(target, dashboard, view, reload_task, key).await?;
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

fn agent_lines(
    target: &InspectionTarget,
    report: AgentsReport,
) -> Vec<bitrouter_tui::dashboard::AgentLine> {
    let can_launch = target.allows_agent_launch();
    report
        .agents
        .into_iter()
        .map(|row| {
            let harness = crate::harness::by_id(&row.id);
            let acp = can_launch
                && harness.map_or(row.configured, |harness| harness.acp_command.is_some());
            let native =
                can_launch && harness.is_some_and(|harness| harness.interactive_binary.is_some());
            bitrouter_tui::dashboard::AgentLine {
                id: match row.id.as_str() {
                    "claude-acp" => "claude".to_string(),
                    "codex-acp" => "codex".to_string(),
                    _ => row.id,
                },
                native,
                acp,
                configured: row.configured,
                description: row.description,
            }
        })
        .collect()
}

async fn open_session(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    request: SessionRequest,
) -> Result<()> {
    let source = target.local_source().ok_or_else(|| {
        bitrouter_sdk::BitrouterError::bad_request(
            "remote ACP conversations are not available; use the operations views",
        )
    })?;
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
    // The live shell cannot relinquish the terminal for an external
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
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    driver: &mut SessionDriver,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    key: KeyEvent,
) -> Result<()> {
    if !dashboard.can_launch_agents {
        match key.code {
            KeyCode::Enter => {
                dashboard.error = Some(
                    "Remote agent catalogs are read-only; start ACP sessions on the local host."
                        .to_string(),
                );
            }
            _ => handle_navigation_key(target, dashboard, view, key).await,
        }
        return Ok(());
    }
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
            if !dashboard
                .agents
                .get(dashboard.selected_agent)
                .is_some_and(|agent| agent.acp)
            {
                dashboard.error = Some("Select an ACP-capable local agent first.".to_string());
                return Ok(());
            }
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
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let (Some(handle), Some(turn)) = (driver.handle.as_ref(), driver.turn.take()) {
            turn.abort();
            handle.client.deny_session_permissions(&handle.session_id);
            handle.client.cancel(&handle.session_id).await?;
            dashboard.conversation.status = "cancelled".to_string();
        }
        return Ok(());
    }
    // Keep a draft editable during a turn, but submission waits for the agent.
    if driver.turn.is_some()
        && key.code == KeyCode::Enter
        && !key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
    {
        return Ok(());
    }
    let Some(bitrouter_tui::dashboard::Effect::Prompt(prompt)) =
        bitrouter_tui::dashboard::step(dashboard, bitrouter_tui::dashboard::Action::Key(key))
    else {
        return Ok(());
    };
    let Some(handle) = driver.handle.as_ref() else {
        dashboard.error = Some("Select an ACP agent before sending a prompt.".to_string());
        dashboard.conversation.input.paste(&prompt);
        return Ok(());
    };
    dashboard
        .conversation
        .journal
        .apply(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(prompt.clone())),
        )));
    dashboard.conversation.status = "working".to_string();
    let client = handle.client.clone();
    let session_id = handle.session_id.clone();
    driver.turn = Some(tokio::spawn(async move {
        client.prompt(&session_id, &prompt).await
    }));
    Ok(())
}

async fn handle_navigation_key(
    target: &InspectionTarget,
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
        KeyCode::Char('8') => view.set_page(bitrouter_tui::dashboard::Page::Providers),
        KeyCode::Char('9') => view.set_page(bitrouter_tui::dashboard::Page::Telemetry),
        KeyCode::Char('0') => view.set_page(bitrouter_tui::dashboard::Page::Policy),
        KeyCode::Char('r') => refresh(target, dashboard).await,
        _ => {}
    }
}

async fn handle_reload_key(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    reload_task: &mut Option<ReloadTask>,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Enter => {
            start_reload(target, dashboard, reload_task);
            Ok(())
        }
        _ => {
            handle_navigation_key(target, dashboard, view, key).await;
            Ok(())
        }
    }
}

async fn handle_policy_key(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    view: &mut bitrouter_tui::dashboard::DashboardView,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Up => {
            dashboard.selected_policy = dashboard.selected_policy.saturating_sub(1);
            dashboard.policy_detail = None;
            dashboard.policy_scroll = 0;
        }
        KeyCode::Down => {
            let count = dashboard
                .policy
                .as_ref()
                .map_or(0, |policy| policy.policies.len());
            dashboard.selected_policy = dashboard
                .selected_policy
                .saturating_add(1)
                .min(count.saturating_sub(1));
            dashboard.policy_detail = None;
            dashboard.policy_scroll = 0;
        }
        KeyCode::Enter => load_selected_policy_detail(target, dashboard).await,
        KeyCode::Char('v') if key.modifiers.is_empty() => {
            dashboard.policy_view = dashboard.policy_view.toggle();
            dashboard.policy_detail = None;
            dashboard.policy_scroll = 0;
            refresh_policy(target, dashboard).await;
        }
        KeyCode::PageUp => {
            dashboard.policy_scroll = dashboard.policy_scroll.saturating_sub(8);
        }
        KeyCode::PageDown => {
            dashboard.policy_scroll = dashboard.policy_scroll.saturating_add(8);
        }
        _ => handle_navigation_key(target, dashboard, view, key).await,
    }
    Ok(())
}

async fn refresh_policy(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
) {
    let view = requested_policy_view(dashboard);
    apply_policy(
        dashboard,
        target.policy(PolicyInput { view, name: None }).await,
    );
}

async fn load_selected_policy_detail(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
) {
    let name = dashboard
        .policy
        .as_ref()
        .and_then(|policy| policy.policies.get(dashboard.selected_policy))
        .cloned();
    let Some(name) = name else {
        mark_failure(
            &mut dashboard.policy_detail_refresh,
            anyhow::anyhow!("select a policy before requesting its detail"),
        );
        return;
    };
    let view = requested_policy_view(dashboard);
    let result = target
        .policy(PolicyInput {
            view,
            name: Some(name.clone()),
        })
        .await;
    apply_policy_detail(dashboard, &name, result);
}

async fn handle_route_key(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Enter if !dashboard.route_input.trim().is_empty() => {
            let model = dashboard.route_input.trim().to_string();
            match target
                .route(RouteInput {
                    model,
                    prompt: None,
                })
                .await
            {
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

async fn refresh(target: &InspectionTarget, dashboard: &mut bitrouter_tui::dashboard::Dashboard) {
    let policy_view = requested_policy_view(dashboard);
    let (status, models, requests, providers, agents, observe, policy, reload, authority) = tokio::join!(
        target.status(),
        target.models(None),
        target.requests(RequestFilters::with_limit(DASHBOARD_REQUEST_ROWS)),
        target.providers(),
        target.agents(),
        target.observe(),
        target.policy(PolicyInput {
            view: policy_view,
            name: None,
        }),
        target.reload_state(),
        target.control_authority(),
    );

    dashboard.target = target.label();
    dashboard.can_launch_agents = target.allows_agent_launch();
    apply_status(dashboard, status);
    apply_models(dashboard, models);
    apply_requests(dashboard, requests);
    apply_providers(dashboard, providers);
    apply_agents(target, dashboard, agents);
    apply_observe(dashboard, observe);
    apply_policy(dashboard, policy);
    apply_reload_state(dashboard, reload);
    apply_control_authority(dashboard, authority);
}

fn apply_status(dashboard: &mut bitrouter_tui::dashboard::Dashboard, result: Result<StatusReport>) {
    match result {
        Ok(status) => {
            dashboard.connected = status.running;
            dashboard.status = if status.running {
                "running".to_string()
            } else {
                "stopped".to_string()
            };
            dashboard.pid = status.pid;
            dashboard.listen = status.listen;
            dashboard.providers = status.providers;
            dashboard.spend = status.spend.as_ref().and_then(|spend| {
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
            mark_success(&mut dashboard.status_refresh);
        }
        Err(error) => {
            if dashboard.status_refresh.updated_at.is_none() {
                dashboard.connected = false;
                dashboard.status = "unavailable".to_string();
            }
            mark_failure(&mut dashboard.status_refresh, error);
        }
    }
}

fn apply_models(dashboard: &mut bitrouter_tui::dashboard::Dashboard, result: Result<ModelsReport>) {
    match result {
        Ok(report) => {
            dashboard.models = report
                .models
                .into_iter()
                .map(|model| bitrouter_tui::dashboard::ModelLine {
                    id: model.id,
                    providers: model.providers.join(", "),
                })
                .collect();
            mark_success(&mut dashboard.models_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.models_refresh, error),
    }
}

fn apply_requests(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<RequestsReport>,
) {
    match result {
        Ok(report) => {
            dashboard.requests = report
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
            mark_success(&mut dashboard.requests_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.requests_refresh, error),
    }
}

fn apply_providers(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<ProvidersReport>,
) {
    match result {
        Ok(report) => {
            dashboard.provider_inventory = report
                .providers
                .into_iter()
                .map(|provider| bitrouter_tui::dashboard::ProviderLine {
                    id: provider.id,
                    models: provider.models,
                    active: provider.active,
                })
                .collect();
            mark_success(&mut dashboard.providers_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.providers_refresh, error),
    }
}

fn apply_agents(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<AgentsReport>,
) {
    match result {
        Ok(report) => {
            dashboard.agents = agent_lines(target, report);
            dashboard.selected_agent = dashboard
                .selected_agent
                .min(dashboard.agents.len().saturating_sub(1));
            mark_success(&mut dashboard.agents_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.agents_refresh, error),
    }
}

fn apply_observe(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<ObserveReport>,
) {
    match result {
        Ok(report) => {
            dashboard.telemetry = Some(bitrouter_tui::dashboard::TelemetryLine {
                daemon_reachable: report.daemon_reachable,
                compiled_in: report.compiled_in,
                exporter_wired: report.exporter_wired,
                sampler: sampler_text(&report),
                metrics_enabled: report.metrics_enabled,
                header_count: report.header_count,
                resource_attribute_count: report.resource_attribute_count,
                api_key_count: report.api_key_count,
                api_key_cap: report.api_key_cap,
                user_id_count: report.user_id_count,
                user_id_cap: report.user_id_cap,
                active_spans: report.active_spans,
            });
            mark_success(&mut dashboard.telemetry_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.telemetry_refresh, error),
    }
}

fn apply_policy(dashboard: &mut bitrouter_tui::dashboard::Dashboard, result: Result<PolicyReport>) {
    match result {
        Ok(report) => {
            let selected_name = dashboard
                .policy
                .as_ref()
                .and_then(|policy| policy.policies.get(dashboard.selected_policy))
                .cloned();
            let view = report.view;
            let policies = report.policies;
            let selected_policy = match selected_name
                .as_ref()
                .and_then(|name| policies.iter().position(|candidate| candidate == name))
            {
                Some(index) => index,
                None => dashboard
                    .selected_policy
                    .min(policies.len().saturating_sub(1)),
            };
            if dashboard
                .policy_detail
                .as_ref()
                .is_some_and(|detail| !policies.contains(&detail.name))
            {
                dashboard.policy_detail = None;
                dashboard.policy_scroll = 0;
            }
            dashboard.policy = Some(bitrouter_tui::dashboard::PolicyLine {
                view: policy_view_text(view).to_string(),
                availability: report.availability,
                digest: report.digest,
                mode: report.mode,
                policies,
                bindings: report.bindings.into_iter().collect(),
            });
            dashboard.policy_view = policy_view_selection(view);
            dashboard.selected_policy = selected_policy;
            mark_success(&mut dashboard.policy_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.policy_refresh, error),
    }
}

fn apply_policy_detail(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    name: &str,
    result: Result<PolicyReport>,
) {
    match result {
        Ok(mut report) => {
            let detail = report.definitions.remove(name).ok_or_else(|| {
                anyhow::anyhow!("policy detail response did not contain the selected policy")
            });
            match detail {
                Ok(detail) => {
                    dashboard.policy_detail = Some(policy_detail_line(name.to_string(), detail));
                    dashboard.policy_scroll = 0;
                    mark_success(&mut dashboard.policy_detail_refresh);
                }
                Err(error) => mark_failure(&mut dashboard.policy_detail_refresh, error),
            }
        }
        Err(error) => mark_failure(&mut dashboard.policy_detail_refresh, error),
    }
}

fn apply_control_authority(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<ControlAuthority>,
) {
    match result {
        Ok(authority) => {
            dashboard.control_read_authorized = Some(authority.read);
            dashboard.control_reload_authorized = Some(authority.reload);
            mark_success(&mut dashboard.authority_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.authority_refresh, error),
    }
}

fn policy_detail_line(
    name: String,
    detail: PolicyDetail,
) -> bitrouter_tui::dashboard::PolicyDetailLine {
    bitrouter_tui::dashboard::PolicyDetailLine {
        name,
        tiers: detail
            .tiers
            .into_iter()
            .map(|(tier, target)| bitrouter_tui::dashboard::PolicyTierLine {
                tier,
                target: target.to_string(),
            })
            .collect(),
        routes: detail
            .routes
            .into_iter()
            .map(|(route, tier)| bitrouter_tui::dashboard::PolicyRouteLine { route, tier })
            .collect(),
        default_tier: detail.default_tier,
        tool_use_tier: detail.tool_use_tier,
        tool_safe_tiers: detail.tool_safe_tiers,
        certificates: detail
            .certificates
            .into_iter()
            .map(
                |(name, certificate)| bitrouter_tui::dashboard::PolicyCertificateLine {
                    name,
                    selected_tier: certificate.selected_tier,
                    evidence_digest: certificate.evidence_digest,
                    compiler_config_digest: certificate.compiler_config_digest,
                    evaluator_config_digest: certificate.evaluator_config_digest,
                },
            )
            .collect(),
    }
}

fn requested_policy_view(dashboard: &bitrouter_tui::dashboard::Dashboard) -> PolicyView {
    match dashboard.policy_view {
        bitrouter_tui::dashboard::PolicyViewSelection::Active => PolicyView::Active,
        bitrouter_tui::dashboard::PolicyViewSelection::Disk => PolicyView::Disk,
    }
}

fn policy_view_selection(view: PolicyView) -> bitrouter_tui::dashboard::PolicyViewSelection {
    match view {
        PolicyView::Active => bitrouter_tui::dashboard::PolicyViewSelection::Active,
        PolicyView::Disk => bitrouter_tui::dashboard::PolicyViewSelection::Disk,
    }
}

fn apply_reload_state(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    result: Result<ReloadState>,
) {
    match result {
        Ok(state) => {
            dashboard.reload = Some(reload_line(state));
            mark_success(&mut dashboard.reload_refresh);
        }
        Err(error) => mark_failure(&mut dashboard.reload_refresh, error),
    }
}

fn start_reload(
    target: &InspectionTarget,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    reload_task: &mut Option<ReloadTask>,
) {
    if reload_task.is_some() {
        return;
    }
    if dashboard.control_reload_authorized == Some(false) {
        mark_failure(
            &mut dashboard.reload_action,
            anyhow::anyhow!(
                "this control credential does not grant reload; refresh capability discovery after its scope changes"
            ),
        );
        return;
    }

    dashboard.reload_in_flight = true;
    dashboard.reload_action.error = None;
    let (sender, updates) = tokio::sync::mpsc::unbounded_channel();
    let target = target.clone();
    let handle = tokio::spawn(async move {
        run_reload_task(target, sender).await;
    });
    *reload_task = Some(ReloadTask { updates, handle });
}

async fn next_reload_update(reload_task: &mut Option<ReloadTask>) -> Option<ReloadUpdate> {
    match reload_task {
        Some(task) => task.updates.recv().await,
        None => std::future::pending().await,
    }
}

async fn run_reload_task(
    target: InspectionTarget,
    updates: tokio::sync::mpsc::UnboundedSender<ReloadUpdate>,
) {
    if target.is_remote() {
        run_remote_reload_task(target, updates).await;
        return;
    }

    match target.reload().await {
        Ok(ReloadSubmission::Local(state)) => {
            let _ = updates.send(ReloadUpdate::LocalFinished(state));
        }
        Ok(ReloadSubmission::Remote(_)) => {
            let _ = updates.send(ReloadUpdate::Failed(
                "local reload action resolved to a remote operation".to_string(),
            ));
        }
        Err(error) => {
            let _ = updates.send(ReloadUpdate::Failed(error.to_string()));
        }
    }
}

async fn run_remote_reload_task(
    target: InspectionTarget,
    updates: tokio::sync::mpsc::UnboundedSender<ReloadUpdate>,
) {
    let state = match target.reload_state().await {
        Ok(state) => state,
        Err(error) => {
            let _ = updates.send(ReloadUpdate::Failed(error.to_string()));
            return;
        }
    };
    let input = ReloadInput {
        request_id: uuid::Uuid::new_v4().to_string(),
        expected_server_instance_id: state.server_instance_id.clone(),
        expected_generation: state.generation,
    };
    if updates
        .send(ReloadUpdate::Prepared {
            request_id: input.request_id.clone(),
            server_instance_id: input.expected_server_instance_id.clone(),
            generation_before: input.expected_generation,
        })
        .is_err()
    {
        return;
    }

    let mut report = match target.submit_remote_reload(&input).await {
        Ok(report) => report,
        Err(error) => {
            let _ = updates.send(ReloadUpdate::Failed(format!(
                "reload submission may be unknown for request {} on instance {}; inspect \
                 operations/{}?instance={}: {error}",
                input.request_id,
                input.expected_server_instance_id,
                input.request_id,
                input.expected_server_instance_id,
            )));
            return;
        }
    };
    if updates
        .send(ReloadUpdate::RemoteSubmitted(report.clone()))
        .is_err()
    {
        return;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while report.is_running() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(250))).await;
        match target
            .operation(&input.request_id, &input.expected_server_instance_id)
            .await
        {
            Ok(next) => report = next,
            Err(error) => {
                let _ = updates.send(ReloadUpdate::Failed(format!(
                    "reload outcome may be unknown for request {} on instance {}; inspect \
                     operations/{}?instance={}: {error}",
                    input.request_id,
                    input.expected_server_instance_id,
                    input.request_id,
                    input.expected_server_instance_id,
                )));
                return;
            }
        }
    }
    let _ = updates.send(ReloadUpdate::RemoteFinished(report));
}

fn apply_reload_update(dashboard: &mut bitrouter_tui::dashboard::Dashboard, update: ReloadUpdate) {
    match update {
        ReloadUpdate::Prepared {
            request_id,
            server_instance_id,
            generation_before,
        } => {
            dashboard.reload_operation = Some(bitrouter_tui::dashboard::ReloadOperationLine {
                lookup: format!("operations/{request_id}?instance={server_instance_id}"),
                request_id,
                server_instance_id,
                generation_before,
                status: "submitting".to_string(),
                completed_at_unix_ms: None,
            });
        }
        ReloadUpdate::RemoteSubmitted(operation) => {
            dashboard.reload_operation = Some(operation_line(operation));
        }
        ReloadUpdate::RemoteFinished(operation) => {
            let diagnostic = operation_diagnostic(&operation);
            dashboard.reload_operation = Some(operation_line(operation));
            dashboard.reload_in_flight = false;
            finish_reload_action(dashboard, diagnostic);
        }
        ReloadUpdate::LocalFinished(state) => {
            let diagnostic = reload_state_diagnostic(&state);
            dashboard.reload = Some(reload_line(state));
            mark_success(&mut dashboard.reload_refresh);
            dashboard.reload_in_flight = false;
            finish_reload_action(dashboard, diagnostic);
        }
        ReloadUpdate::Failed(error) => {
            dashboard.reload_in_flight = false;
            mark_failure(&mut dashboard.reload_action, anyhow::anyhow!(error));
        }
    }
}

fn finish_reload_action(
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
    diagnostic: Option<String>,
) {
    match diagnostic {
        Some(diagnostic) => mark_failure(&mut dashboard.reload_action, anyhow::anyhow!(diagnostic)),
        None => mark_success(&mut dashboard.reload_action),
    }
}

fn reload_line(state: ReloadState) -> bitrouter_tui::dashboard::ReloadLine {
    let (last_outcome, participants, restart_required_fields) =
        state.last_outcome.as_ref().map_or_else(
            || (None, Vec::new(), Vec::new()),
            |report| {
                (
                    Some(reload_outcome_text(report.outcome).to_string()),
                    report
                        .participants
                        .iter()
                        .map(
                            |participant| bitrouter_tui::dashboard::ReloadParticipantLine {
                                participant: reload_participant_text(participant.participant)
                                    .to_string(),
                                outcome: reload_participant_outcome_text(participant.outcome)
                                    .to_string(),
                                detail: participant
                                    .error
                                    .as_ref()
                                    .map(|error| format!("{}: {}", error.code, error.message)),
                            },
                        )
                        .collect(),
                    report.restart_required_fields.clone(),
                )
            },
        );
    bitrouter_tui::dashboard::ReloadLine {
        server_instance_id: state.server_instance_id,
        generation: state.generation,
        running: state.running,
        running_generation: state.running_generation,
        consistency: reload_consistency_text(state.consistency).to_string(),
        last_outcome,
        participants,
        restart_required_fields,
        mixed_state_history: state.mixed_state_history.len(),
    }
}

fn operation_line(report: OperationReport) -> bitrouter_tui::dashboard::ReloadOperationLine {
    bitrouter_tui::dashboard::ReloadOperationLine {
        request_id: report.request_id,
        server_instance_id: report.server_instance_id,
        generation_before: report.generation_before,
        status: operation_status_text(report.status).to_string(),
        lookup: report.lookup_url,
        completed_at_unix_ms: report.completed_at_unix_ms,
    }
}

fn reload_state_diagnostic(state: &ReloadState) -> Option<String> {
    if state.consistency == ReloadConsistency::Mixed {
        return Some(
            "reload left a mixed runtime state; inspect participant results before retrying"
                .to_string(),
        );
    }
    state
        .last_outcome
        .as_ref()
        .and_then(|report| match report.outcome {
            ReloadOutcome::Succeeded => None,
            ReloadOutcome::Failed => Some(
                "reload failed before applying every requested change; inspect participant results"
                    .to_string(),
            ),
            ReloadOutcome::PartiallyApplied => Some(
                "reload partially applied; inspect participant results before retrying".to_string(),
            ),
            ReloadOutcome::Unknown => {
                Some("reload outcome is unknown; inspect current state before retrying".to_string())
            }
        })
}

fn operation_diagnostic(report: &OperationReport) -> Option<String> {
    match report.status {
        OperationStatus::Succeeded => None,
        OperationStatus::Running => Some(format!(
            "reload is still running; inspect {} before submitting another reload",
            report.lookup_url
        )),
        OperationStatus::Failed => Some(format!(
            "reload failed; inspect {} before retrying",
            report.lookup_url
        )),
        OperationStatus::PartiallyApplied => Some(format!(
            "reload partially applied; inspect {} before retrying",
            report.lookup_url
        )),
        OperationStatus::Unknown => Some(format!(
            "reload outcome is unknown; inspect {} before retrying",
            report.lookup_url
        )),
    }
}

fn reload_consistency_text(consistency: ReloadConsistency) -> &'static str {
    match consistency {
        ReloadConsistency::Consistent => "consistent",
        ReloadConsistency::Mixed => "mixed",
    }
}

fn reload_outcome_text(outcome: ReloadOutcome) -> &'static str {
    match outcome {
        ReloadOutcome::Succeeded => "succeeded",
        ReloadOutcome::Failed => "failed",
        ReloadOutcome::PartiallyApplied => "partially_applied",
        ReloadOutcome::Unknown => "unknown",
    }
}

fn reload_participant_text(participant: ReloadParticipant) -> &'static str {
    match participant {
        ReloadParticipant::RoutingTable => "routing_table",
        ReloadParticipant::UpstreamTimeoutClients => "upstream_timeout_clients",
        ReloadParticipant::PolicyTable => "policy_table",
        ReloadParticipant::NamedPolicyRuntime => "named_policy_runtime",
        ReloadParticipant::AccessPolicyStore => "access_policy_store",
    }
}

fn reload_participant_outcome_text(outcome: ReloadParticipantOutcome) -> &'static str {
    match outcome {
        ReloadParticipantOutcome::Applied => "applied",
        ReloadParticipantOutcome::Unchanged => "unchanged",
        ReloadParticipantOutcome::Failed => "failed",
        ReloadParticipantOutcome::NotAttempted => "not_attempted",
    }
}

fn operation_status_text(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::PartiallyApplied => "partially_applied",
        OperationStatus::Unknown => "unknown",
    }
}

fn sampler_text(report: &ObserveReport) -> Option<String> {
    report
        .sampler
        .as_ref()
        .map(|sampler| match report.sampler_arg {
            Some(argument) => format!("{sampler} ({argument})"),
            None => sampler.clone(),
        })
}

fn policy_view_text(view: PolicyView) -> &'static str {
    match view {
        PolicyView::Active => "active",
        PolicyView::Disk => "disk",
    }
}

fn mark_success(state: &mut bitrouter_tui::dashboard::RefreshState) {
    state.updated_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    state.error = None;
}

fn mark_failure(state: &mut bitrouter_tui::dashboard::RefreshState, error: anyhow::Error) {
    state.error = Some(error.to_string());
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

    #[tokio::test]
    async fn a_busy_composer_keeps_one_turn_and_preserves_the_draft() -> Result<()> {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard::default();
        dashboard.conversation.input.paste("first line");
        let mut driver = SessionDriver {
            turn: Some(tokio::spawn(std::future::pending())),
            ..Default::default()
        };
        let turn_id = driver.turn.as_ref().map(tokio::task::JoinHandle::id);
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            handle_conversation_key(
                &mut dashboard,
                &mut driver,
                KeyEvent::new(KeyCode::Enter, modifiers),
            )
            .await?;
            assert_eq!(dashboard.conversation.input.line(), "first line");
            assert_eq!(
                driver.turn.as_ref().map(tokio::task::JoinHandle::id),
                turn_id
            );
        }
        handle_conversation_key(
            &mut dashboard,
            &mut driver,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        )
        .await?;
        assert_eq!(dashboard.conversation.input.line(), "first line\n");
        if let Some(turn) = driver.turn.take() {
            turn.abort();
        }
        Ok(())
    }

    #[test]
    fn control_inventory_dashboard_contract_matches_reachable_pages() -> anyhow::Result<()> {
        use crate::remote_control::inventory::{self, Action};
        use bitrouter_tui::dashboard::Page;

        let contracts = [
            (Action::Status, "overview", Page::Home),
            (Action::Models, "models", Page::Models),
            (Action::Route, "preview", Page::Route),
            (Action::Requests, "requests", Page::Requests),
            (Action::Providers, "providers", Page::Providers),
            (Action::Observe, "telemetry", Page::Telemetry),
            (Action::PolicyStatus, "policy", Page::Policy),
            (Action::PolicyShow, "policy", Page::Policy),
            (Action::Agents, "agents", Page::Agents),
            (Action::Reload, "reload", Page::Reload),
        ];
        if inventory::ACTIONS.len() != contracts.len() {
            anyhow::bail!(
                "control inventory has {} actions but the dashboard contract maps {}",
                inventory::ACTIONS.len(),
                contracts.len()
            );
        }

        for (action, label, page) in contracts {
            let mut matches = inventory::ACTIONS.iter().filter(|row| row.action == action);
            let row = matches.next().ok_or_else(|| {
                anyhow::anyhow!("control inventory has no dashboard contract for {action:?}")
            })?;
            if matches.next().is_some() {
                anyhow::bail!("control inventory has multiple dashboard contracts for {action:?}");
            }
            if row.dashboard != label {
                anyhow::bail!(
                    "control action `{}` advertises dashboard `{}`, expected `{label}`",
                    row.id,
                    row.dashboard
                );
            }

            let mut current = Page::Home;
            let mut reachable = false;
            for _ in 0..=contracts.len() {
                if current == page {
                    reachable = true;
                    break;
                }
                current = current.next();
            }
            if !reachable {
                anyhow::bail!(
                    "control action `{}` names an unreachable dashboard page `{label}`",
                    row.id
                );
            }
        }
        Ok(())
    }

    #[test]
    fn failed_panel_refresh_keeps_other_panels_and_diagnostics() {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard {
            error: Some("Could not start agent".to_string()),
            notice: Some("routing fallback".to_string()),
            models: vec![bitrouter_tui::dashboard::ModelLine {
                id: "active-model".to_string(),
                providers: "primary".to_string(),
            }],
            providers_refresh: bitrouter_tui::dashboard::RefreshState {
                updated_at: Some("2026-09-08T00:00:00Z".to_string()),
                error: None,
            },
            ..Default::default()
        };

        mark_failure(
            &mut dashboard.providers_refresh,
            anyhow::anyhow!("provider read interrupted"),
        );

        assert_eq!(dashboard.error.as_deref(), Some("Could not start agent"));
        assert_eq!(dashboard.notice.as_deref(), Some("routing fallback"));
        assert_eq!(dashboard.models[0].id, "active-model");
        assert_eq!(
            dashboard.providers_refresh.error.as_deref(),
            Some("provider read interrupted")
        );
        assert_eq!(
            dashboard.providers_refresh.updated_at.as_deref(),
            Some("2026-09-08T00:00:00Z")
        );
    }

    #[test]
    fn refreshed_capability_scope_reenables_reload() {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard::default();

        apply_control_authority(
            &mut dashboard,
            Ok(ControlAuthority {
                read: true,
                reload: false,
            }),
        );
        assert_eq!(dashboard.control_reload_authorized, Some(false));

        apply_control_authority(
            &mut dashboard,
            Ok(ControlAuthority {
                read: true,
                reload: true,
            }),
        );
        assert_eq!(dashboard.control_read_authorized, Some(true));
        assert_eq!(dashboard.control_reload_authorized, Some(true));
        assert!(dashboard.authority_refresh.error.is_none());
    }

    #[test]
    fn reload_transport_failure_keeps_the_prepared_lookup() -> anyhow::Result<()> {
        let mut dashboard = bitrouter_tui::dashboard::Dashboard {
            reload_in_flight: true,
            ..Default::default()
        };
        apply_reload_update(
            &mut dashboard,
            ReloadUpdate::Prepared {
                request_id: "request-id".to_string(),
                server_instance_id: "instance-id".to_string(),
                generation_before: 7,
            },
        );
        apply_reload_update(
            &mut dashboard,
            ReloadUpdate::Failed("transport disconnected".to_string()),
        );

        let Some(operation) = dashboard.reload_operation.as_ref() else {
            return Err(anyhow::anyhow!("prepared reload operation was lost"));
        };
        assert_eq!(operation.request_id, "request-id");
        assert_eq!(operation.server_instance_id, "instance-id");
        assert_eq!(operation.status, "submitting");
        assert!(operation.lookup.contains("request-id?instance=instance-id"));
        assert!(!dashboard.reload_in_flight);
        assert_eq!(
            dashboard.reload_action.error.as_deref(),
            Some("transport disconnected")
        );
        Ok(())
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
