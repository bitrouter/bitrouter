//! The shared, asynchronous interactive conversation driver.

use std::collections::VecDeque;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, CurrentModeUpdate, SessionConfigOption, SessionUpdate, StopReason,
};
use anyhow::{Context, Result, ensure};
use bitrouter_tui::code::{
    CodeAction, CodeEffect, CodeState, CodeStatus, CodeView, Command, CommandOwner, CommandTarget,
    Inspector, Selector, TurnOutcome,
};
use crossterm::event::EventStream;
use futures::{FutureExt, StreamExt, future::LocalBoxFuture};
use tokio_util::sync::CancellationToken;

use crate::acp_cli::{SessionHandle, SessionSelection};
use crate::actions::code::{CodeServices, OpenedSession};
use crate::dashboard::SessionRequest;

use super::code_controls::{picker, row};
use super::code_wire::{CodeWire, WireEvent};

enum JobResult {
    Opened(Box<OpenedSession>),
    Selected {
        handle: Box<SessionHandle>,
        result: Result<()>,
        resumed: bool,
    },
    Selector(Selector),
    Report {
        title: String,
        content: String,
    },
    Route(String),
    Config(Vec<SessionConfigOption>),
    Mode(String),
    Edited(Result<String, String>),
    Copied,
}

struct Runtime {
    services: Arc<CodeServices>,
    state: CodeState,
    status: CodeStatus,
    wire: CodeWire,
    job: Option<LocalBoxFuture<'static, Result<JobResult>>>,
    session_job_cancel: Option<CancellationToken>,
    job_blocks_prompt: bool,
    effects: VecDeque<CodeEffect>,
    templates: Vec<bitrouter_tui::machine::PromptCommand>,
    selectors: Vec<Selector>,
    editing: bool,
    exit: bool,
    cleanup: futures::stream::FuturesUnordered<LocalBoxFuture<'static, bool>>,
    teardown_failed: bool,
    route_probe: Option<LocalBoxFuture<'static, Result<String>>>,
    disconnected_details: Option<String>,
    mutation_selector: Option<String>,
}

pub(crate) async fn run(
    services: Arc<CodeServices>,
    initial: Option<SessionRequest>,
) -> Result<()> {
    let status = CodeStatus {
        title: services.label.clone(),
        ..Default::default()
    };
    let mut runtime = Runtime {
        state: CodeState::new(status.clone()),
        status,
        services,
        wire: CodeWire::default(),
        job: None,
        session_job_cancel: None,
        job_blocks_prompt: false,
        effects: VecDeque::new(),
        templates: Vec::new(),
        selectors: Vec::new(),
        editing: false,
        exit: false,
        cleanup: Default::default(),
        teardown_failed: false,
        route_probe: None,
        disconnected_details: None,
        mutation_selector: None,
    };
    runtime
        .state
        .set_operations_only(runtime.services.operations_only);
    runtime.refresh_commands();
    if let Some(request) = initial {
        runtime.start(request);
    } else if runtime.services.operations_only {
        runtime.report("status", Vec::new());
    } else {
        runtime.effects.push_back(CodeEffect::ChooseAgent);
    }
    let mut view = CodeView::open().context("opening Code terminal")?;
    let result = runtime.drive(&mut view).await;
    // Session jobs own controlled children. Cancel their lifecycle RPC and
    // recover the handle for teardown; ordinary effects can be dropped.
    if let Some(cancel) = runtime.session_job_cancel.take() {
        cancel.cancel();
    } else {
        runtime.job = None;
    }
    let mut session_job_error = None;
    if let Some(job) = runtime.job.take() {
        match job.await {
            Ok(JobResult::Opened(opened)) => runtime.wire.attach(opened.handle),
            Ok(JobResult::Selected { handle, .. }) => runtime.wire.reattach(*handle),
            Ok(_) => {}
            Err(error) if crate::acp_cli::is_lifecycle_cancelled(&error) => {}
            Err(error) => session_job_error = Some(error),
        }
    }
    let clean = runtime.wire.shutdown().await;
    while let Some(cleanup) = runtime.cleanup.next().await {
        runtime.teardown_failed |= !cleanup;
    }
    let restored = view.finish();
    result?;
    restored?;
    if let Some(error) = session_job_error {
        return Err(error);
    }
    ensure!(
        clean && !runtime.teardown_failed,
        "ACP teardown did not confirm; inspect the session log"
    );
    Ok(())
}

impl Runtime {
    async fn drive(&mut self, view: &mut CodeView) -> Result<()> {
        let mut events = Some(EventStream::new());
        let mut shutdown = super::signals::Shutdown::install();
        let mut dirty = true;
        let mut paint_due = None;
        loop {
            while let Some(effect) = self.effects.pop_front() {
                let selection = match &effect {
                    CodeEffect::Select { selector, .. } => Some(selector.clone()),
                    _ => None,
                };
                let submitted = match &effect {
                    CodeEffect::Submit { prompt } | CodeEffect::AgentPrompt { prompt } => {
                        Some(prompt.clone())
                    }
                    _ => None,
                };
                if let Err(error) = self.effect(effect) {
                    if self.job.is_none() {
                        self.job_blocks_prompt = false;
                    }
                    if let Some(prompt) = submitted {
                        self.effects
                            .extend(self.state.step(CodeAction::SubmissionRejected {
                                prompt,
                                reason: format!("{error:#}"),
                            }));
                    } else {
                        if let Some(selector) = selection {
                            self.state.selector_mutation_failed(&selector);
                        }
                        self.state.open_inspector(Inspector::new(
                            "Operation failed",
                            format!("{error:#}"),
                        ));
                    }
                }
                dirty = true;
            }
            if self.exit {
                return Ok(());
            }
            if self.editing && events.is_some() {
                // Drop the input stream before handing the terminal to the editor.
                events = None;
                view.suspend()?;
            }
            if dirty && !self.editing {
                view.draw(&mut self.state)?;
                dirty = false;
                paint_due = None;
            }
            tokio::select! {
                event = next_input(&mut events) => {
                    let Some(event) = event else { return Ok(()); };
                    self.effects.extend(self.state.step(CodeAction::Event(event?)));
                    dirty = true;
                }
                event = self.wire.next() => {
                    let streamed = matches!(&event, WireEvent::Update(_));
                    self.wire_event(event);
                    if streamed {
                        paint_due.get_or_insert(tokio::time::Instant::now() + bitrouter_tui::writer::Schedule::INTERVAL);
                    } else { dirty = true; }
                }
                result = next_job(&mut self.job) => {
                    self.job = None;
                    self.session_job_cancel = None;
                    let blocked_prompt = self.job_blocks_prompt;
                    self.job_blocks_prompt = false;
                    let mutation = self.mutation_selector.take();
                    if self.editing {
                        view.resume()?;
                        events = Some(EventStream::new());
                        self.editing = false;
                    }
                    match result {
                        Ok(result) => {
                            if let Some(selector) = mutation {
                                self.state.selector_mutation_succeeded(&selector);
                            }
                            self.job_result(result);
                        }
                        Err(error) => {
                            if let Some(selector) = mutation {
                                self.state.selector_mutation_failed(&selector);
                            }
                            self.state.open_inspector(Inspector::new("Operation failed", format!("{error:#}")));
                            // A report can finish while the agent is working or
                            // awaiting permission. Preserve that activity.
                            if blocked_prompt && self.wire.handle.is_none() {
                                self.status = self.state.status().clone();
                                self.status.activity = "disconnected".into();
                                self.state.set_status(self.status.clone());
                            }
                            self.refresh_commands();
                        }
                    }
                    dirty = true;
                }
                _ = shutdown.recv() => return Ok(()),
                route = next_route(&mut self.route_probe) => {
                    self.route_probe = None;
                    match route {
                        Ok(route) => {
                            self.status = self.state.status().clone();
                            self.status.route = route;
                            self.state.set_status(self.status.clone());
                        }
                        Err(error) => self.state.open_inspector(Inspector::new(
                            "Session route unavailable", format!("{error:#}"),
                        )),
                    }
                    dirty = true;
                }
                clean = self.cleanup.next(), if !self.cleanup.is_empty() => {
                    if clean == Some(false) { self.teardown_failed = true; self.state.set_notice("ACP teardown did not confirm; inspect the session log"); }
                    dirty = true;
                }
                () = paint_at(paint_due), if !self.editing => { dirty = true; },
            }
        }
    }

    fn start(&mut self, request: SessionRequest) {
        self.retain_session_details();
        self.job_blocks_prompt = true;
        self.route_probe = None;
        let services = self.services.clone();
        let cancel = CancellationToken::new();
        self.session_job_cancel = Some(cancel.clone());
        let mut previous = std::mem::take(&mut self.wire);
        self.state.set_session_active(false);
        self.status.activity = format!("connecting · {}", request.agent);
        self.state.set_status(self.status.clone());
        self.job = Some(
            async move {
                ensure!(
                    previous.shutdown().await,
                    "Previous ACP session teardown did not confirm"
                );
                Ok(JobResult::Opened(Box::new(
                    services.start_with_cancel(request, &cancel).await?,
                )))
            }
            .boxed_local(),
        );
    }

    fn report(&mut self, id: &str, args: Vec<String>) {
        self.job_blocks_prompt = false;
        let services = self.services.clone();
        let id = id.to_string();
        self.job = Some(
            async move {
                let content = services.report(&id, &args).await?;
                Ok(JobResult::Report {
                    title: report_title(&id).to_string(),
                    content,
                })
            }
            .boxed_local(),
        );
    }

    fn selector(&mut self, selector: Selector) {
        let id = selector.id.clone();
        self.selectors.retain(|old| old.id != id);
        self.selectors.push(selector);
        self.state.set_selectors(self.selectors.clone());
        self.state.open_selector(&id);
    }

    fn idle(&self) -> Result<()> {
        ensure!(
            !self.wire.working() && !self.wire.has_permissions(),
            "Wait for the current turn and permissions to settle"
        );
        Ok(())
    }

    fn effect(&mut self, effect: CodeEffect) -> Result<()> {
        let explicit_agent = matches!(&effect, CodeEffect::AgentPrompt { .. });
        match effect {
            CodeEffect::Submit { prompt } | CodeEffect::AgentPrompt { prompt } => {
                ensure!(
                    !self.services.operations_only,
                    "This target is operations-only"
                );
                ensure!(
                    !self.job_blocks_prompt && self.cleanup.is_empty(),
                    "Wait for the session operation to finish; your draft is retained"
                );
                if explicit_agent {
                    let head = prompt
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .trim_start_matches('/');
                    ensure!(
                        self.state
                            .journal()
                            .commands()
                            .iter()
                            .any(|command| command.name.trim_start_matches('/') == head),
                        "This agent command is no longer advertised"
                    );
                }
                self.wire.submit(prompt)?;
                self.effects
                    .extend(self.state.step(CodeAction::TurnStarted));
                return Ok(());
            }
            CodeEffect::Exit => {
                self.exit = true;
                return Ok(());
            }
            CodeEffect::Cancel => {
                self.wire.cancel();
                return Ok(());
            }
            CodeEffect::ResolvePermission { id, outcome } => {
                self.wire.resolve(&id, outcome);
                return Ok(());
            }
            _ => {}
        }
        ensure!(
            self.job.is_none(),
            "An operation is still running; your draft is retained"
        );
        ensure!(
            self.cleanup.is_empty(),
            "Waiting for the previous ACP connection to close; your draft is retained"
        );
        match effect {
            CodeEffect::ChooseAgent => {
                self.idle()?;
                ensure!(
                    self.state.queue_len() == 0,
                    "Edit or discard queued work before changing agents"
                );
                let services = self.services.clone();
                self.job = Some(
                    async move {
                        let rows = services
                            .agents()
                            .await?
                            .into_iter()
                            .map(|agent| row(&agent.id, &agent.id, agent.description))
                            .collect();
                        Ok(JobResult::Selector(picker("agent", "Choose agent", rows)))
                    }
                    .boxed_local(),
                );
            }
            CodeEffect::OpenSession => self.open_sessions()?,
            CodeEffect::Settings => {
                self.idle()?;
                ensure!(self.wire.handle.is_some(), "Choose an agent first");
                self.refresh_settings();
                self.state.open_selector("settings");
            }
            CodeEffect::Report { id } => {
                if id == "details" {
                    self.details();
                } else if id == "route" {
                    self.selector(
                        picker("preview", "Route preview", Vec::new())
                            .allow_custom("Model to preview"),
                    );
                } else if id == "policy_show" {
                    self.selector(
                        picker("policy", "Policy detail", Vec::new()).allow_custom("Policy name"),
                    );
                } else {
                    self.report(&id, Vec::new());
                }
            }
            CodeEffect::LocalAction { action, args } => self.local_action(&action, args)?,
            CodeEffect::Select {
                selector,
                id,
                custom,
            } => self.selected(&selector, id, custom)?,
            CodeEffect::ExternalEditor => {
                self.idle()?;
                self.job_blocks_prompt = true;
                let draft = self.state.editor().text().to_string();
                self.editing = true;
                self.job = Some(
                    async move {
                        Ok(JobResult::Edited(
                            super::editor::edit(draft)
                                .await
                                .map_err(|error| format!("{error:#}")),
                        ))
                    }
                    .boxed_local(),
                );
            }
            CodeEffect::Copy { text } => {
                self.job = Some(
                    async move {
                        super::editor::copy(&text).await?;
                        Ok(JobResult::Copied)
                    }
                    .boxed_local(),
                );
            }
            CodeEffect::Exit
            | CodeEffect::Cancel
            | CodeEffect::ResolvePermission { .. }
            | CodeEffect::Submit { .. }
            | CodeEffect::AgentPrompt { .. } => {}
        }
        Ok(())
    }

    fn local_action(&mut self, action: &str, args: Vec<String>) -> Result<()> {
        if matches!(action, "route_set" | "route_reset") {
            self.idle()?;
            self.job_blocks_prompt = true;
            self.route_probe = None;
            let handle = self.wire.handle.as_ref().context("Choose an agent first")?;
            let client = handle.client.clone();
            let session = handle.session_id.clone();
            let action = action.to_string();
            self.job = Some(
                async move {
                    if action == "route_reset" {
                        ensure!(args.is_empty(), "usage: /route reset");
                        client.route_reset(&session).await?;
                        Ok(JobResult::Route("default (no override)".into()))
                    } else if let Some(route) = args.first() {
                        Ok(JobResult::Route(client.route_set(&session, route).await?))
                    } else {
                        let routes = client.route_list(&session).await?;
                        let rows = routes
                            .available
                            .into_iter()
                            .map(|route| row(&route, &route, "Session route"))
                            .collect();
                        Ok(JobResult::Selector(picker("route", "Session route", rows)))
                    }
                }
                .boxed_local(),
            );
        } else if action == "commands" {
            self.state.set_notice(
                "Ctrl-P opens commands; slash completion labels each command's owner".to_string(),
            );
        } else {
            self.report(action, args);
        }
        Ok(())
    }

    fn open_sessions(&mut self) -> Result<()> {
        self.idle()?;
        ensure!(
            self.state.queue_len() == 0,
            "Resolve queued work before opening another session"
        );
        let handle = self.wire.handle.as_ref().context("Choose an agent first")?;
        let mut rows = Vec::new();
        if handle.capabilities.load {
            rows.push(row("load", "Load session", "Replays native history"));
        }
        if handle.capabilities.resume {
            rows.push(row(
                "resume",
                "Resume session",
                "Earlier history is not replayed",
            ));
        }
        let mut select = picker("session_method", "Open native session", rows);
        if select.rows.is_empty() {
            select.detail = "This agent does not advertise load or resume".into();
        }
        self.selector(select);
        Ok(())
    }

    fn selected(&mut self, selector: &str, id: String, custom: bool) -> Result<()> {
        match selector {
            "agent" => {
                self.idle()?;
                ensure!(
                    self.state.queue_len() == 0,
                    "Resolve queued work before changing agents"
                );
                self.start(SessionRequest {
                    agent: id,
                    selection: SessionSelection::New,
                    turn_timeout: None,
                    routing: Default::default(),
                });
            }
            "preview" => self.report("route", vec![id]),
            "policy" => self.report("policy_show", vec![id]),
            "route" => self.local_action("route_set", vec![id])?,
            "settings" => {
                self.state.open_selector(&id);
            }
            "session_method" => self.session_page(&id, None)?,
            "load" | "resume" => {
                self.idle()?;
                self.route_probe = None;
                ensure!(
                    self.state.queue_len() == 0,
                    "Resolve queued work before opening another session"
                );
                if !custom && let Some(cursor) = id.strip_prefix("page:") {
                    return self.session_page(selector, Some(cursor.to_string()));
                }
                let id = if custom {
                    id
                } else {
                    id.strip_prefix("session:")
                        .context("Invalid native session row")?
                        .to_string()
                };
                let mut handle = self.wire.handle.take().context("Choose an agent first")?;
                self.job_blocks_prompt = true;
                let resumed = selector == "resume";
                let selection = if resumed {
                    SessionSelection::Resume(id)
                } else {
                    SessionSelection::Load(id)
                };
                self.status.activity = "opening native session".into();
                self.state.set_status(self.status.clone());
                let cancel = CancellationToken::new();
                self.session_job_cancel = Some(cancel.clone());
                self.job = Some(
                    async move {
                        let result = handle.select_with_cancel(&selection, &cancel).await;
                        Ok(JobResult::Selected {
                            handle: Box::new(handle),
                            result,
                            resumed,
                        })
                    }
                    .boxed_local(),
                );
            }
            "mode" => {
                self.idle()?;
                self.job_blocks_prompt = true;
                let handle = self.wire.handle.as_ref().context("Choose an agent first")?;
                ensure!(
                    handle
                        .initial_settings
                        .modes
                        .as_ref()
                        .is_some_and(|modes| modes
                            .available_modes
                            .iter()
                            .any(|mode| mode.id.to_string() == id)),
                    "This mode is no longer advertised"
                );
                let client = handle.client.clone();
                let session = handle.session_id.clone();
                self.job = Some(
                    async move {
                        client.set_session_mode(&session, id.clone()).await?;
                        Ok(JobResult::Mode(id))
                    }
                    .boxed_local(),
                );
            }
            _ if selector.starts_with("config:") => {
                self.idle()?;
                self.job_blocks_prompt = true;
                let config_id = selector.trim_start_matches("config:").to_string();
                let option = self
                    .state
                    .journal()
                    .config()
                    .iter()
                    .find(|option| option.id.to_string() == config_id)
                    .context("This setting is no longer available")?;
                let value = super::code_controls::setting_value(option, &id)?;
                let handle = self.wire.handle.as_ref().context("Choose an agent first")?;
                let client = handle.client.clone();
                let session = handle.session_id.clone();
                self.job = Some(
                    async move {
                        let response = client
                            .set_session_config_option(&session, config_id, value)
                            .await?;
                        Ok(JobResult::Config(response.config_options))
                    }
                    .boxed_local(),
                );
            }
            _ => anyhow::bail!("Unknown selection surface `{selector}`"),
        }
        if selector == "route" || selector == "mode" || selector.starts_with("config:") {
            self.mutation_selector = Some(selector.to_string());
        }
        Ok(())
    }

    fn session_page(&mut self, method: &str, cursor: Option<String>) -> Result<()> {
        let handle = self.wire.handle.as_ref().context("Choose an agent first")?;
        let mut select = picker(
            method,
            format!(
                "{} native session",
                if method == "load" { "Load" } else { "Resume" }
            ),
            Vec::new(),
        )
        .allow_custom("Native session ID");
        if !handle.capabilities.list {
            self.selector(select);
            return Ok(());
        }
        let client = handle.client.clone();
        self.job = Some(
            async move {
                let response = client
                    .list_sessions(Some(std::env::current_dir()?), cursor)
                    .await?;
                select.rows = response
                    .sessions
                    .into_iter()
                    .map(|session| {
                        row(
                            format!("session:{}", session.session_id),
                            session
                                .title
                                .unwrap_or_else(|| session.session_id.to_string()),
                            session.cwd.display().to_string(),
                        )
                    })
                    .collect();
                if let Some(cursor) = response.next_cursor {
                    select.rows.push(row(
                        format!("page:{cursor}"),
                        "More sessions…",
                        "Next native result page",
                    ));
                }
                Ok(JobResult::Selector(select))
            }
            .boxed_local(),
        );
        Ok(())
    }

    fn job_result(&mut self, result: JobResult) {
        let settings_changed = matches!(&result, JobResult::Config(_) | JobResult::Mode(_));
        match result {
            JobResult::Opened(opened) => {
                self.disconnected_details = None;
                self.templates = opened.prompt_commands;
                self.state.reset_session();
                self.status.agent = opened.handle.agent_id.clone();
                self.status.route = if opened.handle.via.is_some() {
                    "unreported".into()
                } else {
                    "direct".into()
                };
                self.status.activity = "ready".into();
                self.state.set_status(self.status.clone());
                self.state.set_session_active(true);
                if opened.resumed {
                    self.state.set_notice("Earlier history was not replayed");
                }
                if !opened.diagnostics.is_empty() {
                    self.state.set_notice(opened.diagnostics.join("\n"));
                }
                self.wire.attach(opened.handle);
                self.seed_initial_settings();
                self.refresh_settings();
                self.refresh_commands();
                self.refresh_route();
            }
            JobResult::Selected {
                handle,
                result,
                resumed,
            } => {
                match result {
                    Ok(()) => {
                        self.disconnected_details = None;
                        self.wire.reattach(*handle);
                        self.state.reset_session();
                        self.seed_initial_settings();
                        self.state.set_session_active(true);
                        self.status.route = "unreported".into();
                        self.refresh_route();
                        if resumed {
                            self.state.set_notice("Earlier history was not replayed");
                        }
                    }
                    Err(error) => {
                        self.wire.handle = Some(*handle);
                        self.state
                            .set_notice(format!("Could not open native session: {error:#}"));
                    }
                }
                self.status.activity = "ready".into();
                self.state.set_status(self.status.clone());
                self.refresh_settings();
                self.refresh_commands();
            }
            JobResult::Selector(selector) => self.selector(selector),
            JobResult::Report { title, content } => {
                let inspector = Inspector { title, content };
                if self.services.operations_only && title_is_root(&inspector.title) {
                    self.state.operations_root(inspector);
                } else {
                    self.state.open_inspector(inspector);
                }
            }
            JobResult::Route(route) => {
                self.status.route = route;
                self.state.set_status(self.status.clone());
            }
            JobResult::Config(options) => {
                self.state
                    .apply(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                        options,
                    )))
            }
            JobResult::Mode(id) => self
                .state
                .apply(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(id))),
            JobResult::Edited(result) => self
                .effects
                .extend(self.state.step(CodeAction::ExternalEditorFinished(result))),
            JobResult::Copied => self.state.set_notice("Copied"),
        }
        if settings_changed {
            self.refresh_settings();
            self.refresh_commands();
        }
    }

    fn wire_event(&mut self, event: WireEvent) {
        match event {
            WireEvent::Update(update) => {
                let commands = matches!(update, SessionUpdate::AvailableCommandsUpdate(_));
                let settings = matches!(
                    update,
                    SessionUpdate::ConfigOptionUpdate(_) | SessionUpdate::CurrentModeUpdate(_)
                );
                self.state.apply(update);
                if settings {
                    self.refresh_settings();
                }
                if commands || settings {
                    self.refresh_commands();
                }
            }
            WireEvent::Permission(permission) => {
                let context = permission.tool_call;
                let prompt = bitrouter_tui::permission::Prompt::new(
                    permission.request_id,
                    context.fields.title.clone(),
                    context.tool_call_id.to_string(),
                    context.fields.kind,
                    permission.options,
                );
                self.effects
                    .extend(self.state.receive_permission_with_context(prompt, context));
            }
            WireEvent::Settled(result) => {
                let outcome = match result {
                    Ok(response) => match response.stop_reason {
                        StopReason::EndTurn => TurnOutcome::Completed,
                        StopReason::Cancelled => TurnOutcome::Cancelled,
                        reason => TurnOutcome::Stopped(format!("{reason:?}")),
                    },
                    Err(error) => TurnOutcome::Failed(format!("{error:#}")),
                };
                self.effects
                    .extend(self.state.step(CodeAction::TurnSettled(outcome)));
            }
            WireEvent::CancellationExpired | WireEvent::Disconnected => {
                self.retain_session_details();
                let activity = if matches!(event, WireEvent::CancellationExpired) {
                    "disconnected · cancellation did not settle"
                } else {
                    "disconnected · adapter closed"
                };
                self.route_probe = None;
                self.effects.extend(
                    self.state
                        .step(CodeAction::TurnSettled(TurnOutcome::Disconnected)),
                );
                let mut wire = std::mem::take(&mut self.wire);
                self.cleanup
                    .push(async move { wire.shutdown().await }.boxed_local());
                self.state.set_session_active(false);
                self.status = self.state.status().clone();
                self.status.activity = activity.into();
                self.state.set_status(self.status.clone());
                self.refresh_commands();
            }
            WireEvent::CancelFailed(error) => self.state.set_notice(format!(
                "Cancellation request failed: {error}; waiting for settlement"
            )),
        }
    }

    fn details(&mut self) {
        let content = self
            .live_session_details()
            .or_else(|| {
                self.disconnected_details.as_ref().map(|details| {
                    format!("Disconnected session · retained for inspection\n\n{details}")
                })
            })
            .unwrap_or_else(|| "No ACP session is connected".into());
        self.state.open_inspector(Inspector {
            title: "Session details".into(),
            content,
        });
    }

    fn retain_session_details(&mut self) {
        if let Some(details) = self.live_session_details() {
            self.disconnected_details = Some(details);
        }
    }

    fn live_session_details(&self) -> Option<String> {
        self.wire.handle.as_ref().map(|handle| {
            let usage = self.state.journal().usage();
            let context = usage.filter(|usage| usage.size > 0).map(|usage| {
                format!("{} / {} tokens (agent-reported)", usage.used, usage.size)
            }).unwrap_or_else(|| "unreported".into());
            let mut content = format!(
                "Agent: {}\nNative session: {}\nProvider session: {}\nRoute: {}\nLifecycle: {}\nContext usage: {context}",
                handle.agent_id,
                handle.session_id,
                handle.agent_session_id.as_deref().unwrap_or("unreported"),
                self.state.status().route,
                handle.capabilities.lifecycle_summary(),
            );
            if let Some(usage) = usage && let Ok(raw) = serde_json::to_string_pretty(usage) {
                content.push_str("\n\nReported usage and pricing metadata:\n");
                content.push_str(&raw);
            }
            content
        })
    }

    fn seed_initial_settings(&mut self) {
        if let Some(handle) = &self.wire.handle {
            for update in handle.initial_settings.updates() {
                self.state.apply(update);
            }
        }
    }

    fn refresh_settings(&mut self) {
        let Some(handle) = self.wire.handle.as_ref() else {
            return;
        };
        let mut modes = handle.initial_settings.modes.clone();
        if let Some(modes) = &mut modes
            && let Some(current) = self.state.journal().mode()
        {
            modes.current_mode_id = current.clone();
        }
        self.selectors.retain(|selector| {
            selector.id != "settings"
                && selector.id != "mode"
                && !selector.id.starts_with("config:")
        });
        self.selectors.extend(super::code_controls::settings(
            self.state.journal().config(),
            modes.as_ref(),
        ));
        self.state.set_selectors(self.selectors.clone());
    }

    fn refresh_route(&mut self) {
        let Some(handle) = self.wire.handle.as_ref() else {
            return;
        };
        if handle.via.is_none() {
            self.status.route = "direct".into();
            return;
        }
        if !handle
            .client
            .route_control()
            .allows(bitrouter_sdk::acp::client::RouteMethod::List)
        {
            return;
        }
        let client = handle.client.clone();
        let session = handle.session_id.clone();
        self.route_probe = Some(
            async move {
                Ok(client
                    .route_list(&session)
                    .await?
                    .current
                    .unwrap_or_else(|| "default (no override)".into()))
            }
            .boxed_local(),
        );
    }

    fn refresh_commands(&mut self) {
        let mut commands = Vec::new();
        if !self.services.operations_only {
            let handle = self.wire.handle.as_ref();
            commands.push(Command::new(
                "Choose agent",
                "Open an ACP agent",
                CommandOwner::BitRouter,
                CommandTarget::ChooseAgent,
            ));
            let mut sessions = Command::new(
                "Open session",
                "Load or resume a native session",
                CommandOwner::BitRouter,
                CommandTarget::OpenSession,
            );
            if let Some(handle) = handle {
                if !handle.capabilities.load && !handle.capabilities.resume {
                    sessions = sessions.unavailable("This agent does not advertise load or resume");
                }
            } else {
                sessions = sessions.unavailable("Choose an agent first");
            }
            commands.push(sessions);
            let mut settings = Command::new(
                "Agent settings",
                "Settings reported by the agent",
                CommandOwner::BitRouter,
                CommandTarget::Settings,
            );
            if let Some(handle) = handle {
                if self.state.journal().config().is_empty()
                    && handle
                        .initial_settings
                        .modes
                        .as_ref()
                        .is_none_or(|modes| modes.available_modes.is_empty())
                {
                    settings =
                        settings.unavailable("No supported agent settings have been reported");
                }
            } else {
                settings = settings.unavailable("Choose an agent first");
            }
            commands.push(settings);
            let mut details = Command::new(
                "Session details",
                "Native identity and reported usage",
                CommandOwner::BitRouter,
                CommandTarget::Report {
                    id: "details".into(),
                },
            );
            if handle.is_none() && self.disconnected_details.is_none() {
                details = details.unavailable("Choose an agent first");
            }
            commands.push(details);
        }
        for (label, detail, id) in [
            ("Status", "Selected target status", "status"),
            ("Routable models", "Configured model catalog", "list_models"),
            (
                "Host requests",
                "Latest 100 host requests; not session-filtered",
                "requests",
            ),
            (
                "Route preview",
                "Configured resolution for a model",
                "route",
            ),
            ("Providers", "Accepted provider inventory", "providers_list"),
            ("Telemetry", "Live exporter status", "observe_status"),
            ("Policy status", "Active policy summary", "policy_status"),
            (
                "Policy detail",
                "Inspect a named active policy",
                "policy_show",
            ),
            (
                "Agent catalog",
                "Configured and catalog agents",
                "agents_list",
            ),
            (
                "Reload state",
                "Coordinator generation and outcome",
                "reload_state",
            ),
            ("Reload now", "Submit a live configuration reload", "reload"),
        ] {
            let mut command = Command::new(
                label,
                detail,
                CommandOwner::BitRouter,
                CommandTarget::Report { id: id.into() },
            );
            if matches!(id, "reload" | "reload_state") && !self.services.can_reload {
                command = command.unavailable("Selected credential has no reload scope");
            }
            commands.push(command);
        }
        let typed = self
            .services
            .commands(self.wire.handle.as_ref().map(|handle| &handle.client));
        self.state
            .set_typed_commands(typed.clone(), self.templates.clone());
        {
            for command in typed {
                let mut row = Command::new(
                    format!("/{}", command.name),
                    command.summary,
                    CommandOwner::BitRouter,
                    CommandTarget::LocalAction {
                        action: command.action.into(),
                        args: Vec::new(),
                    },
                );
                if let Some(reason) = command.unavailable {
                    row = row.unavailable(reason);
                }
                commands.push(row);
            }
        }
        for command in &self.templates {
            commands.push(Command::new(
                format!("/{}", command.name),
                &command.description,
                CommandOwner::PromptTemplate,
                CommandTarget::PromptTemplate {
                    prompt: command.template.clone(),
                },
            ));
        }
        self.state.set_commands(commands);
    }
}

fn title_is_root(title: &str) -> bool {
    title == "Target status"
}

fn report_title(id: &str) -> &str {
    match id {
        "status" => "Target status",
        "requests" => "Host requests · latest 100",
        "list_models" => "Routable models",
        "route" => "Route preview · configured resolution",
        "providers_list" => "Providers · accepted inventory",
        "observe_status" => "Telemetry · live exporter",
        "policy_status" => "Policy · active status",
        "policy_show" => "Policy · active detail",
        "agents_list" => "Agent catalog",
        "reload_state" => "Reload · coordinator state",
        "reload" => "Reload · submitted operation",
        _ => id,
    }
}

async fn next_input(
    events: &mut Option<EventStream>,
) -> Option<std::io::Result<crossterm::event::Event>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

async fn next_job(
    job: &mut Option<LocalBoxFuture<'static, Result<JobResult>>>,
) -> Result<JobResult> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}

async fn paint_at(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn next_route(probe: &mut Option<LocalBoxFuture<'static, Result<String>>>) -> Result<String> {
    match probe {
        Some(probe) => probe.await,
        None => std::future::pending().await,
    }
}
