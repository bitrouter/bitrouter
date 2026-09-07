//! Application driver for the local/remote operations dashboard.

use std::path::{Path, PathBuf};

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
    refresh(&target, &mut dashboard).await;

    let mut view =
        bitrouter_tui::dashboard::DashboardView::open().context("opening operations dashboard")?;
    let outcome = drive(&target, &mut dashboard, &mut view).await;
    view.finish();
    outcome
}

async fn drive(
    target: &Target,
    dashboard: &mut bitrouter_tui::dashboard::Dashboard,
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
                if handle_event(target, dashboard, view, event).await? {
                    return Ok(());
                }
                view.draw(dashboard).context("drawing operations dashboard")?;
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
    view: &mut bitrouter_tui::dashboard::DashboardView,
    event: Event,
) -> Result<bool> {
    match event {
        Event::Resize(_, _) => {}
        Event::Paste(text) if view.page() == bitrouter_tui::dashboard::Page::Route => {
            dashboard
                .route_input
                .push_str(&text.replace(['\r', '\n'], " "));
        }
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            if exit_key(&key, view.page()) {
                return Ok(true);
            }
            if key.code == KeyCode::Tab {
                view.next_page();
                return Ok(false);
            }
            if view.page() == bitrouter_tui::dashboard::Page::Route {
                handle_route_key(target, dashboard, key).await?;
            } else {
                match key.code {
                    KeyCode::Char('1') => view.set_page(bitrouter_tui::dashboard::Page::Overview),
                    KeyCode::Char('2') => view.set_page(bitrouter_tui::dashboard::Page::Models),
                    KeyCode::Char('3') => view.set_page(bitrouter_tui::dashboard::Page::Requests),
                    KeyCode::Char('4') => view.set_page(bitrouter_tui::dashboard::Page::Route),
                    KeyCode::Char('r') => refresh(target, dashboard).await,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn exit_key(key: &KeyEvent, page: bitrouter_tui::dashboard::Page) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        || (page != bitrouter_tui::dashboard::Page::Route && key.code == KeyCode::Char('q'))
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
            *dashboard = dashboard_from_reports(target.label(), status, models, requests);
            dashboard.route_input = route_input;
            dashboard.route = route;
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
