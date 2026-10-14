//! Local Goal UI client. The daemon is the only execution/state authority.
mod client;
mod outbox;
mod projection;
mod submissions;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use bitrouter_orchestrator::goal::{
    GoalCommand, GoalOperation, GoalPage, GoalReply, GoalSnapshot, HistoryPage, RunIntent,
};
use bitrouter_tui::goals::{Control, Effect, Screen, State};
use client::{Client, Pending};
use crossterm::event::EventStream;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Default)]
struct Directory {
    after: u64,
    cutoff: Option<u64>,
    next: Option<u64>,
    previous: Vec<u64>,
    epoch: u64,
}

enum Job {
    Directory(u64, Result<GoalPage>),
    Snapshot(String, Result<Box<GoalSnapshot>>),
    History(String, Result<HistoryPage>),
    Prepared(Effect, Result<Pending>),
    Submitted(Pending, Result<GoalReply>),
    Inspected(String, String, Result<String>),
    Edited(Option<String>, Result<String>),
}

pub async fn run(
    config: Option<&Path>,
    socket_override: Option<&Path>,
    workspace_override: Option<PathBuf>,
    goal_id: Option<String>,
) -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "BRO Goals requires an interactive terminal; use bro run <agent> for headless execution"
    );
    let source = crate::paths::resolve_config(config)?;
    let cfg = crate::paths::load_config(&source).await?;
    let socket = socket_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::daemon::socket_path_for(&source, &cfg));
    if socket_override.is_none() {
        // Startup is infrastructure only; no Goal, harness or provider request.
        if crate::daemon::probe_status(&socket).await?.is_none() {
            match crate::daemon::start_and_wait(
                &source,
                &source.home().join("bitrouter.log"),
                Some(&socket),
                Duration::from_secs(15),
            )
            .await?
            {
                crate::daemon::DaemonStartOutcome::Ready(_) => {}
                crate::daemon::DaemonStartOutcome::Exited { status, log_tail } => {
                    anyhow::bail!("Goal daemon exited ({status}): {log_tail}")
                }
                crate::daemon::DaemonStartOutcome::NotReadyInTime { pid } => {
                    anyhow::bail!(
                        "Goal daemon startup is still unconfirmed (PID {pid}); no second daemon was launched"
                    )
                }
            }
        }
    }
    let workspace =
        tokio::fs::canonicalize(workspace_override.unwrap_or(std::env::current_dir()?)).await?;
    let client = Client::new(socket.clone());
    let outbox = outbox::Outbox::new(source.home().join("goal-ui-outbox"), socket);
    let mut state = State::new(workspace.display().to_string());
    let mut directory = Directory::default();
    let GoalReply::Directory { page } = client
        .request(GoalCommand::List {
            after: 0,
            cutoff: None,
            limit: 16,
        })
        .await?
    else {
        anyhow::bail!("unexpected Goal directory reply");
    };
    let mut artifacts = projection::Artifacts::new();
    show_page(&mut state, &mut directory, page, &mut artifacts);
    if let Some(id) = goal_id {
        state.observe(id);
    }
    let root = source.home().join("goal-workspaces");
    let mut view = bitrouter_tui::goals::view::View::open()?;
    let mut events = Some(EventStream::new());
    let mut shutdown = crate::chat::signals::Shutdown::install();
    let mut jobs = JoinSet::new();
    let mut editing = false;
    let mut directory_poll = false;
    let mut snapshot_poll = false;
    let mut history_poll = false;
    let recovered = outbox.recover().await?;
    let recovered_count = recovered.len();
    let mut drafts = BTreeMap::new();
    let mut submissions = submissions::Submissions::default();
    for pending in recovered {
        if let Some(text) = &pending.text {
            drafts.insert(pending.target.clone(), pending.id()?.to_owned());
            if let Some(target) = &pending.target {
                state
                    .notebooks
                    .entry(target.clone())
                    .or_default()
                    .editor
                    .set_text(text);
            } else {
                state.new_editor.set_text(text);
            }
        }
        submissions.retain(pending)?;
    }
    submissions.sync(&mut state);
    if recovered_count > 0 {
        state.notice = Some(
            "Recovered unresolved client operations; reading original receipts without replay"
                .into(),
        );
    }
    let mut history: BTreeMap<String, u64> = BTreeMap::new();
    let mut inputs: BTreeMap<String, String> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if !editing {
            view.draw(&mut state)?;
        }
        tokio::select! {
            result = jobs.join_next(), if !jobs.is_empty() => {
                match result {
                    Some(Ok(Job::Edited(target,result))) => {
                        editing = false;
                        view.resume()?;
                        events = Some(EventStream::new());
                        match result {
                            Ok(text) => {
                                if let Some(id) = target { state.notebooks.entry(id).or_default().editor.set_text(text); }
                                else { state.new_editor.set_text(text); }
                                state.notice = Some("External editor returned; draft retained until Enter".into());
                            },
                            Err(error) => state.notice = Some(format!("External editor failed: {error:#}")),
                        }
                    },
                    Some(Ok(Job::Directory(epoch,result))) => {
                        directory_poll = false;
                        if epoch == directory.epoch { match result {
                            Ok(page) => show_page(&mut state,&mut directory,page,&mut artifacts),
                            Err(error) => state.notice = Some(format!("Directory stale / unavailable: {error}")),
                        } }
                    }
                    Some(Ok(Job::Snapshot(id,result))) => {
                        snapshot_poll = false;
                        match result { Ok(goal) => {
                            if let Some(input) = &goal.inputs.latest && inputs.get(&goal.id) != Some(&input.input_id) {
                                for line in projection::input_lines(&goal) { state.event_line(&goal.id,line); }
                                inputs.insert(goal.id.clone(),input.input_id.clone());
                            }
                            state.receive(projection::goal(&goal,&mut artifacts));
                        }, Err(error) => state.notice = Some(format!("Goal {id} snapshot stale: {error}")) }
                    }
                    Some(Ok(Job::History(id,result))) => {
                        history_poll = false;
                        match result {
                            Ok(page) => for operation in page.operations {
                                let cursor = history.entry(id.clone()).or_default();
                                if operation.revision > *cursor { if let Some(line) = projection::operation(&operation) { state.event_line(&id,line); } *cursor = operation.revision; }
                            },
                            Err(error) => state.notice = Some(format!("History unavailable for {id}: {error}")),
                        }
                    }
                    Some(Ok(Job::Prepared(effect,result))) => match result {
                        Ok(pending) => {
                            match outbox.save(&pending).await {
                                Ok(()) => {
                                    if pending.text.is_some() { drafts.insert(pending.target.clone(),pending.id()?.to_owned()); }
                                    submissions.sync(&mut state); submit(&mut jobs,&client,pending);
                                },
                                Err(error) => { submissions.finish(&pending)?; submissions.sync(&mut state); state.notice = Some(format!("No mutation sent: could not persist client identity: {error}")); }
                            }
                        },
                        Err(error) => { submissions.finish_effect(&effect); submissions.sync(&mut state); state.notice = Some(format!("No mutation sent; draft retained: {error}")); }
                    },
                    Some(Ok(Job::Submitted(pending,result))) => {
                        submissions.finish(&pending)?;
                        match result {
                            Ok(GoalReply::Accepted { operation }) => {
                                if let Err(error) = pending.verifies(&operation) { state.notice = Some(error.to_string()); submissions.retain(pending)?; submissions.sync(&mut state); continue; }
                                let creating = pending.target.is_none() && state.screen == Screen::New && state.new_pending;
                                let open_created = accept_draft(&mut state,&mut drafts,&pending)? && creating;
                                submissions.sync(&mut state);
                                if open_created { state.observe(operation.goal_id.clone()); }
                                if let Err(error) = outbox.remove(&pending).await { state.notice = Some(format!("Receipt verified; local outbox cleanup pending: {error}")); }
                                state.notice = Some(format!("Operation {} saved for Goal {}; execution feedback follows facts",operation.id,operation.goal_id));
                            }
                            Ok(_) => { state.notice = Some("Mutation returned no receipt; resolving the original identity".into()); submissions.retain(pending)?; submissions.sync(&mut state); }
                            Err(error) if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>().is_some_and(|error| !matches!(error,bitrouter_orchestrator::goal::GoalError::Storage { .. })) => {
                                submissions.sync(&mut state);
                                outbox.remove(&pending).await?;
                                state.notice = Some(format!("Mutation rejected; draft retained: {error}"));
                            }
                            Err(error) => { state.notice = Some(format!("Acceptance unknown; preserving original scope/identity: {error}")); submissions.retain(pending)?; submissions.sync(&mut state); }
                        }
                    }
                    Some(Ok(Job::Inspected(goal,id,result))) => {
                        state.receive_evidence(&goal,&id,result.map_err(|error|error.to_string()));
                    }
                    Some(Err(error)) => { state.notice = Some(format!("UI request job ended: {error}; no execution state inferred")); }
                    None => {}
                }
            }
            _ = tick.tick() => {
                while let Some(pending) = submissions.next()? { reconcile(&mut jobs,&client,pending); }
                if state.screen == Screen::Goals && !directory_poll {
                    directory_poll = true; poll_directory(&mut jobs,&client,&directory);
                }
                if let Some(id) = state.current.clone() {
                    if !snapshot_poll { snapshot_poll = true; poll_snapshot(&mut jobs,&client,id.clone()); }
                    if !history_poll { history_poll = true; poll_history(&mut jobs,&client,id.clone(),*history.get(&id).unwrap_or(&0)); }
                }
            }
            signal = shutdown.recv() => match signal {
                crate::chat::signals::TerminalSignal::Shutdown => break,
                crate::chat::signals::TerminalSignal::Suspend => { drop(events.take()); if !editing { view.suspend()?; } crate::chat::signals::suspend_current_process()?; if !editing { view.resume()?; events = Some(EventStream::new()); } }
            },
            event = async { match events.as_mut() { Some(events) => events.next().await, None => std::future::pending().await } } => {
                let Some(event) = event else { break; }; let event = event?;
                if bitrouter_tui::editor::is_redraw(&event) { view.invalidate(); }
                for effect in state.event(&event) {
                    match effect {
                        Effect::Detach => return Ok(()),
                        Effect::ExternalEditor { goal_id, text } => {
                            drop(events.take());
                            view.suspend()?;
                            editing = true;
                            jobs.spawn(async move { Job::Edited(goal_id, crate::chat::editor::edit(text).await) });
                        },
                        Effect::Observe(id) => { if !snapshot_poll { snapshot_poll = true; poll_snapshot(&mut jobs,&client,id); } },
                        Effect::Refresh => { directory.cutoff = None; directory.epoch = directory.epoch.saturating_add(1); if !directory_poll { directory_poll = true; poll_directory(&mut jobs,&client,&directory); } },
                        Effect::NextPage => if let Some(next) = directory.next { directory.previous.push(directory.after); directory.after = next; directory.epoch = directory.epoch.saturating_add(1); if !directory_poll { directory_poll = true; poll_directory(&mut jobs,&client,&directory); } },
                        Effect::PreviousPage => if let Some(previous) = directory.previous.pop() { directory.after = previous; directory.epoch = directory.epoch.saturating_add(1); if !directory_poll { directory_poll = true; poll_directory(&mut jobs,&client,&directory); } },
                        Effect::Inspect { goal_id, artifact_id } => if let Some(reference) = artifacts.get(&(goal_id.clone(),artifact_id.clone())).cloned() {
                            let permission = state.permission_evidence_is_open(&goal_id,&artifact_id);
                            let root = crate::goal_artifacts::root(&root,&goal_id);
                            jobs.spawn(async move { let result = async {
                                anyhow::ensure!(reference.bytes <= 1024 * 1024, "evidence exceeds the text preview limit; immutable identity remains available");
                                let bytes = crate::goal_artifacts::read(&root,&reference).await?;
                                if permission { projection::permission_text(&bytes) } else { String::from_utf8(bytes).context("binary evidence cannot be shown as text") }
                            }.await; Job::Inspected(goal_id,artifact_id,result) });
                        },
                        other => if !submissions.begin(&other) { state.notice = Some("Request not sent: an earlier request in this scope is unresolved, or the request limit was reached. Draft retained.".into()); }
                        else { submissions.sync(&mut state); let client = client.clone(); let workspace = workspace.clone(); jobs.spawn(async move { let result = client::prepare(&client,&workspace,&other).await; Job::Prepared(other,result) }); }
                    }
                }
            }
        }
    }
    Ok(())
}

fn accept_draft(
    state: &mut State,
    drafts: &mut BTreeMap<Option<String>, String>,
    pending: &Pending,
) -> Result<bool> {
    let operation_id = pending.id()?;
    if let Some(text) = &pending.text
        && drafts
            .get(&pending.target)
            .is_some_and(|id| id == operation_id)
    {
        state.accepted(pending.target.as_deref(), text);
        drafts.remove(&pending.target);
        return Ok(true);
    }
    Ok(false)
}

fn show_page(
    state: &mut State,
    directory: &mut Directory,
    page: GoalPage,
    artifacts: &mut projection::Artifacts,
) {
    directory.cutoff = Some(page.cutoff);
    directory.next = page.next_after;
    state.receive_directory(
        page.goals
            .iter()
            .map(|goal| projection::goal(goal, artifacts))
            .collect(),
        page.next_after.is_some(),
        !directory.previous.is_empty(),
    );
}
fn poll_directory(jobs: &mut JoinSet<Job>, client: &Client, directory: &Directory) {
    let client = client.clone();
    let (after, cutoff, epoch) = (directory.after, directory.cutoff, directory.epoch);
    jobs.spawn(async move {
        Job::Directory(
            epoch,
            async {
                match client
                    .request(GoalCommand::List {
                        after,
                        cutoff,
                        limit: 16,
                    })
                    .await?
                {
                    GoalReply::Directory { page } => Ok(page),
                    _ => anyhow::bail!("unexpected Goal directory reply"),
                }
            }
            .await,
        )
    });
}
fn poll_snapshot(jobs: &mut JoinSet<Job>, client: &Client, id: String) {
    let client = client.clone();
    jobs.spawn(async move {
        let result = async {
            match client
                .request(GoalCommand::Get {
                    goal_id: id.clone(),
                })
                .await?
            {
                GoalReply::Snapshot { goal } => Ok(goal),
                _ => anyhow::bail!("unexpected Goal snapshot reply"),
            }
        }
        .await;
        Job::Snapshot(id, result)
    });
}
fn poll_history(jobs: &mut JoinSet<Job>, client: &Client, id: String, after: u64) {
    let client = client.clone();
    jobs.spawn(async move {
        let result = async {
            match client
                .request(GoalCommand::History {
                    goal_id: id.clone(),
                    after,
                    cutoff: None,
                    limit: 16,
                })
                .await?
            {
                GoalReply::History { page } => Ok(page),
                _ => anyhow::bail!("unexpected Goal history reply"),
            }
        }
        .await;
        Job::History(id, result)
    });
}
fn submit(jobs: &mut JoinSet<Job>, client: &Client, pending: Pending) {
    let client = client.clone();
    jobs.spawn(async move {
        let result = client.request(pending.command.clone()).await;
        Job::Submitted(pending, result)
    });
}
fn reconcile(jobs: &mut JoinSet<Job>, client: &Client, pending: Pending) {
    let client = client.clone();
    jobs.spawn(async move { let result = async {
        match client.request(GoalCommand::Operation { operation_id:pending.id()?.into() }).await {
            Ok(reply) => Ok(reply),
            Err(error) if matches!(error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>(),Some(bitrouter_orchestrator::goal::GoalError::NotFound)) => Err(anyhow::anyhow!("Original operation is not found; store incarnation is not established, so this unresolved admission has not been replayed")),
            Err(error) => Err(error),
        }
    }.await; Job::Submitted(pending,result) });
}
