//! `bitrouter tui` — in-process multi-agent manager.
mod event;
mod highlight;
mod pump;
mod state;
mod ui;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_substrate::engine::Session;
use bitrouter_substrate::translate::PermissionOutcome;
use crossterm::event::{Event as CtEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::tui::event::{AppEvent, DiffData, Effect, Incoming, PermOption};
use crate::tui::state::{AppState, PaneState, reduce};

/// Launch the TUI with an initial agent; `n` (in AGENT mode) spawns more.
pub async fn run(agent_id: &str, worktree: Option<&str>) -> Result<()> {
    // ── Config + catalog. ──
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    let catalog = bitrouter_sdk::acp::ConfigAcpRoutingTable::from_configs(
        cfg.agents.iter().map(|(k, v)| (k.clone(), v.clone())),
    )
    .context("building acp catalog from config.agents")?;
    let agent_ids: Vec<String> = cfg.agents.keys().cloned().collect();
    // Fail a bad --agent id up front with the fix in the message, instead of
    // surfacing the substrate's bare lookup error.
    if !cfg.agents.contains_key(agent_id) {
        let mut available = agent_ids.clone();
        available.sort();
        anyhow::bail!(
            "no acp agent '{agent_id}' under `agents:` in the config — available: {}",
            if available.is_empty() {
                "none configured (add one with `bitrouter agents install <id>`)".to_string()
            } else {
                available.join(", ")
            }
        );
    }
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let ports = (cfg.worktrees.ports.from, cfg.worktrees.ports.to);

    // ── Initial session: the primary agent runs in the base repo (worktree
    // strictly opt-in via --worktree) but still draws a fleet PORT. Worktrees
    // are retained on close (they hold the agent's work); transcripts stay on
    // (LaunchOptions default). ──
    let initial_port = alloc_port(ports, &HashMap::new());
    let options = bitrouter_substrate::engine::LaunchOptions {
        worktree: worktree.map(|name| bitrouter_substrate::worktree::WorktreeSpec {
            name: name.to_string(),
            branch: None,
            remove_on_shutdown: false,
        }),
        env: port_env(initial_port),
        ..Default::default()
    };
    let session = Session::launch(&catalog, agent_id, base_repo.clone(), options)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let record_id = session.state().record_id.clone();
    let session = Arc::new(session);

    // ── Channel + pump. The loop keeps `tx` to pump agents spawned later. ──
    let (tx, rx) = unbounded_channel::<Incoming>();
    pump::spawn(Arc::clone(&session), record_id.clone(), tx.clone());

    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    sessions.insert(record_id.clone(), session);
    let mut port_alloc: HashMap<String, u16> = HashMap::new();
    if let Some(p) = initial_port {
        port_alloc.insert(record_id.clone(), p);
    }

    // ── Initial state. ──
    let mut initial_pane = PaneState::new(record_id, agent_id.to_string());
    initial_pane.port = initial_port;
    let mut state = AppState::new(initial_pane);
    state.bootstrap_cmd = cfg.worktrees.bootstrap.clone();
    state.set_available_agents(agent_ids);
    state.set_harness_map(
        cfg.agents
            .iter()
            .map(|(k, v)| (k.clone(), harness_tag(&v.transport)))
            .collect(),
    );
    // Agents route LLM traffic through `bitrouter serve`; probe it once so a
    // missing proxy is an up-front notice instead of silent agent failures.
    if let Some(warning) = probe_serve(&cfg.server.listen).await {
        state.notice = Some(warning);
    }
    // https://no-color.org: any non-empty value disables foreground colors.
    state.no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    // ── Run; the loop owns full session teardown. The panic hook guarantees
    // the terminal is restored even if the loop panics mid-draw. ──
    install_panic_restore(restore_terminal);
    let mut terminal = setup_terminal().context("entering raw mode")?;
    let rt = Runtime {
        sessions,
        pending: HashMap::new(),
        prompt_tasks: HashMap::new(),
        spawner: Spawner {
            catalog: &catalog,
            base_repo,
            tx,
        },
        fleet: Fleet {
            ports,
            port_alloc,
            meta: HashMap::new(),
            checks: cfg.worktrees.checks.clone(),
        },
    };
    let result = event_loop(&mut terminal, state, rx, rt).await;
    restore_terminal();
    result
}

/// Fleet-level resources the loop allocates per subagent (TUI_SPEC §6).
struct Fleet {
    /// The inclusive `PORT` pool (`worktrees.ports`).
    ports: (u16, u16),
    /// record_id → allocated port; freed when the agent closes.
    port_alloc: HashMap<String, u16>,
    /// record_id → integration metadata for worktree-isolated agents.
    meta: HashMap<String, WorkMeta>,
    /// Verification checks run in the worktree before "ready to review".
    checks: Vec<String>,
}

/// Where a worktree-isolated agent's work lives, for diff/review/merge.
#[derive(Clone)]
struct WorkMeta {
    worktree: std::path::PathBuf,
    branch: String,
    /// The base repo `HEAD` at spawn — the diff/merge base.
    base_ref: String,
}

/// Lowest port in the inclusive pool not currently allocated; `None` when the
/// pool is exhausted (the agent then simply gets no `PORT`).
fn alloc_port(range: (u16, u16), allocated: &HashMap<String, u16>) -> Option<u16> {
    (range.0..=range.1).find(|p| !allocated.values().any(|used| used == p))
}

/// The `PORT` env overlay for a launch, when a port was allocated.
fn port_env(port: Option<u16>) -> Vec<(String, String)> {
    port.map(|p| vec![("PORT".to_string(), p.to_string())])
        .unwrap_or_default()
}

/// Terse harness tag for a pane header: the basename of the agent command
/// (e.g. `/usr/local/bin/claude` → `claude`).
fn harness_tag(transport: &bitrouter_sdk::acp::AcpTransport) -> String {
    match transport {
        bitrouter_sdk::acp::AcpTransport::Stdio { command, .. } => std::path::Path::new(command)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| command.clone()),
    }
}

/// Probe the configured `bitrouter serve` listen address; `Some(warning)` when
/// nothing accepts within 500ms. Wildcard binds probe via loopback.
async fn probe_serve(listen: &str) -> Option<String> {
    let addr = listen.replace("0.0.0.0", "127.0.0.1");
    let connect = tokio::net::TcpStream::connect(&addr);
    match tokio::time::timeout(std::time::Duration::from_millis(500), connect).await {
        Ok(Ok(_)) => None,
        _ => Some(format!(
            "bitrouter serve unreachable at {addr} — agents' LLM calls will fail; start it with `bitrouter serve`"
        )),
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Best-effort terminal restore (raw mode off, leave alt-screen, show cursor).
/// Needs no `Terminal` handle so the panic hook can call it from any thread;
/// errors are ignored — by then we're exiting anyway.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Chain `restore` in front of the current panic hook so a panic anywhere in
/// the TUI (draw, reducer, an effect) restores the user's terminal before the
/// default hook prints the message onto a readable screen.
fn install_panic_restore(restore: fn()) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
    }));
}

/// Bundles what's needed to launch new sessions from inside the loop.
struct Spawner<'a> {
    catalog: &'a bitrouter_sdk::acp::ConfigAcpRoutingTable,
    base_repo: std::path::PathBuf,
    tx: UnboundedSender<Incoming>,
}

/// In-flight prompt tasks keyed by the owning session's `record_id`, so a
/// pane close can abort+await just that session's tasks.
type PromptTasks = HashMap<String, Vec<tokio::task::JoinHandle<()>>>;

/// The live session registry plus everything else `apply_effect` mutates,
/// bundled so the function stays under clippy's `too_many_arguments` threshold.
struct Runtime<'a> {
    sessions: HashMap<String, Arc<Session>>,
    pending: HashMap<String, bitrouter_substrate::up::PendingPermission>,
    prompt_tasks: PromptTasks,
    spawner: Spawner<'a>,
    fleet: Fleet,
}

/// The core loop over a registry of sessions. Draws, muxes input vs pumped
/// events, reduces, and applies effects (including async spawn/close).
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut state: AppState,
    mut rx: UnboundedReceiver<Incoming>,
    mut rt: Runtime<'_>,
) -> Result<()> {
    let mut keys = EventStream::new();
    // Drives the running-agent spinner and gives coalesced batches a steady
    // redraw cadence.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if let Err(e) = terminal.draw(|f| ui::render(&mut state, f)) {
            cleanup(&mut rt).await;
            return Err(e).context("draw");
        }
        if state.should_quit {
            cleanup(&mut rt).await;
            return Ok(());
        }

        // Convert the next input/pumped item into a pure AppEvent.
        let app_event: Option<AppEvent> = tokio::select! {
            maybe_key = keys.next() => match maybe_key {
                Some(Ok(CtEvent::Key(k))) => Some(AppEvent::Key(k)),
                // Resize needs no state change: `None` continues to the top of
                // the loop, whose draw autoresizes to the new size. Mouse is
                // deliberately unhandled.
                Some(Ok(_)) => None,
                Some(Err(_)) | None => Some(AppEvent::Key(quit_key())),
            },
            _ = ticker.tick() => Some(AppEvent::Tick),
            maybe_in = rx.recv() => match maybe_in {
                Some(incoming) => convert_incoming(incoming, &mut rt),
                None => Some(AppEvent::Key(quit_key())),
            },
        };

        if let Some(app_event) = app_event {
            let effects = reduce(&mut state, &app_event);
            for effect in effects {
                apply_effect(effect, &mut state, &mut rt).await;
            }
        }
        // Frame coalescing: a chatty agent can emit hundreds of updates in a
        // burst; fold everything already queued before the next draw so the
        // terminal repaints once per batch, not once per chunk. The cap keeps
        // one agent from starving input indefinitely.
        let mut drained = 0;
        while drained < 256 {
            let incoming = match rx.try_recv() {
                Ok(i) => i,
                Err(_) => break,
            };
            drained += 1;
            if let Some(ev) = convert_incoming(incoming, &mut rt) {
                let effects = reduce(&mut state, &ev);
                for effect in effects {
                    apply_effect(effect, &mut state, &mut rt).await;
                }
            }
        }
        // Reap finished prompt tasks so the map stays bounded over a long session.
        rt.prompt_tasks.retain(|_, handles| {
            handles.retain(|h| !h.is_finished());
            !handles.is_empty()
        });
    }
}

/// Convert one pumped `Incoming` into a pure `AppEvent`, stashing resolvable
/// permission handles in the runtime. Shared by the select arm and the
/// coalescing drain.
fn convert_incoming(incoming: Incoming, rt: &mut Runtime<'_>) -> Option<AppEvent> {
    match incoming {
        Incoming::Update { record_id, update } => Some(AppEvent::Update { record_id, update }),
        Incoming::Permission {
            record_id,
            pending: p,
        } => {
            if rt.sessions.contains_key(&record_id) {
                // Stash the handle; hand the reducer display-only data.
                let ev = AppEvent::Permission {
                    record_id: record_id.clone(),
                    title: p.tool_call.fields.title.clone().unwrap_or_default(),
                    diff: perm_diff(&p),
                    options: perm_options(&p),
                    risk: crate::risk::classify(&p.tool_call.fields, &rt.spawner.base_repo),
                };
                rt.pending.insert(record_id, *p);
                Some(ev)
            } else {
                // Pane already closed; dropping `p` denies the request.
                None
            }
        }
        Incoming::TurnEnded {
            record_id,
            stop_reason,
        } => Some(AppEvent::TurnEnded {
            record_id,
            stop_reason,
        }),
        Incoming::Exited { record_id } => Some(AppEvent::Exited { record_id }),
        Incoming::PromptFailed { record_id, error } => {
            Some(AppEvent::PromptFailed { record_id, error })
        }
        Incoming::ReviewReady {
            record_id,
            files,
            adds,
            dels,
        } => Some(AppEvent::ReviewReady {
            record_id,
            files,
            adds,
            dels,
        }),
        Incoming::ChecksFailed { record_id, output } => {
            Some(AppEvent::ChecksFailed { record_id, output })
        }
        Incoming::DiffLoaded { record_id, text } => Some(AppEvent::DiffLoaded { record_id, text }),
    }
}

/// Apply one reducer effect against the live session registry: send prompts,
/// resolve permissions, and (async) spawn/close sessions.
async fn apply_effect(effect: Effect, state: &mut AppState, rt: &mut Runtime<'_>) {
    match effect {
        Effect::Quit => {}
        Effect::Bell => {
            // Ring the terminal bell (BEL). Best-effort; ignore write errors.
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
        Effect::Prompt { record_id, text } => {
            if let Some(sess) = rt.sessions.get(&record_id) {
                let sess = Arc::clone(sess);
                let tx = rt.spawner.tx.clone();
                let rid = record_id.clone();
                let handle = tokio::spawn(async move {
                    // Send fails only at teardown; ignore either way.
                    match sess.prompt(&text).await {
                        // The typed stop reason feeds working/idle state and
                        // (with a diff) the review queue — don't discard it.
                        Ok(resp) => {
                            let _ = tx.send(Incoming::TurnEnded {
                                record_id: rid,
                                stop_reason: resp.stop_reason,
                            });
                        }
                        // Surface in the pane — a silent failure looks like a
                        // hung agent.
                        Err(e) => {
                            let _ = tx.send(Incoming::PromptFailed {
                                record_id: rid,
                                error: e.to_string(),
                            });
                        }
                    }
                });
                rt.prompt_tasks.entry(record_id).or_default().push(handle);
            }
        }
        Effect::ResolvePermission { record_id, outcome } => {
            if let Some(p) = rt.pending.remove(&record_id) {
                // Map the reducer's y/a/n outcome onto the exact option the
                // upstream offered (validated `optionId`, per ACP).
                let selected = bitrouter_substrate::translate::select_option(outcome, &p.options);
                p.resolve(selected);
            }
        }
        Effect::SpawnAgent { agent_id } => {
            // Fleet-managed subagents are worktree-isolated BY DEFAULT
            // (TUI_SPEC §6): each gets its own worktree + branch
            // (`bitrouter/<agent>-<record16>`, based on the manager's HEAD),
            // a PORT from the pool, and — when the human approved it — the
            // bootstrap hook. Worktrees are retained on close: cleanup is
            // gated on merged-or-discarded, never automatic.
            let tag = crate::fleet_mcp::branch_tag(&agent_id);
            let port = alloc_port(rt.fleet.ports, &rt.fleet.port_alloc);
            let options = bitrouter_substrate::engine::LaunchOptions {
                worktree: Some(bitrouter_substrate::worktree::WorktreeSpec {
                    name: format!("{tag}-{{record16}}"),
                    branch: Some(format!("bitrouter/{tag}-{{record16}}")),
                    remove_on_shutdown: false,
                }),
                worktree_bootstrap: match state.bootstrap_decision {
                    Some(true) => state.bootstrap_cmd.clone(),
                    _ => None,
                },
                env: port_env(port),
                ..Default::default()
            };
            match Session::launch(
                rt.spawner.catalog,
                &agent_id,
                rt.spawner.base_repo.clone(),
                options,
            )
            .await
            {
                Ok(sess) => {
                    let rid = sess.state().record_id.clone();
                    // Integration metadata: where the work lives, on which
                    // branch, against which base — feeds the review queue.
                    if let Some(wt) = sess.worktree_path() {
                        let handle: String = rid.chars().filter(|c| *c != '-').take(16).collect();
                        let base_ref = crate::fleet_mcp::git_stdout(
                            &rt.spawner.base_repo,
                            &["rev-parse", "HEAD"],
                        )
                        .await
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                        rt.fleet.meta.insert(
                            rid.clone(),
                            WorkMeta {
                                worktree: wt.to_path_buf(),
                                branch: format!("bitrouter/{tag}-{handle}"),
                                base_ref,
                            },
                        );
                    }
                    let sess = Arc::new(sess);
                    rt.sessions.insert(rid.clone(), Arc::clone(&sess));
                    if let Some(p) = port {
                        rt.fleet.port_alloc.insert(rid.clone(), p);
                    }
                    pump::spawn(sess, rid.clone(), rt.spawner.tx.clone());
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawned {
                            record_id: rid,
                            agent_id,
                            port,
                        },
                    );
                }
                Err(e) => {
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawnFailed {
                            agent_id,
                            error: e.to_string(),
                        },
                    );
                }
            }
        }
        Effect::CloseAgent { record_id } => {
            rt.pending.remove(&record_id);
            rt.fleet.port_alloc.remove(&record_id);
            rt.fleet.meta.remove(&record_id);
            if let Some(handles) = rt.prompt_tasks.remove(&record_id) {
                for handle in handles {
                    handle.abort();
                    let _ = handle.await;
                }
            }
            if let Some(sess) = rt.sessions.remove(&record_id) {
                shutdown_session(sess).await;
            }
        }
        Effect::CheckReview { record_id } => {
            // Inspect the worktree in the background (checks may run tests);
            // results come back as ReviewReady / ChecksFailed.
            let Some(meta) = rt.fleet.meta.get(&record_id).cloned() else {
                return; // no worktree — nothing to review
            };
            let checks = rt.fleet.checks.clone();
            let tx = rt.spawner.tx.clone();
            tokio::spawn(async move {
                let incoming = inspect_worktree(&record_id, &meta, &checks).await;
                if let Some(incoming) = incoming {
                    let _ = tx.send(incoming);
                }
            });
        }
        Effect::LoadDiff { record_id } => {
            let Some(meta) = rt.fleet.meta.get(&record_id).cloned() else {
                state.notice = Some("no worktree to diff (agent runs in the base repo)".into());
                return;
            };
            let tx = rt.spawner.tx.clone();
            tokio::spawn(async move {
                let text =
                    match crate::fleet_mcp::git_stdout(&meta.worktree, &["diff", &meta.base_ref])
                        .await
                    {
                        Ok(t) if t.trim().is_empty() => {
                            "(no changes vs the spawn base)".to_string()
                        }
                        Ok(t) => t,
                        Err(e) => format!("diff failed: {e:#}"),
                    };
                let _ = tx.send(Incoming::DiffLoaded { record_id, text });
            });
        }
        Effect::Merge { record_id } => {
            // Serialized integration: awaited inline, so one merge lands at a
            // time (merge-queue semantics).
            let Some(meta) = rt.fleet.meta.get(&record_id).cloned() else {
                state.notice = Some("no worktree to merge".into());
                return;
            };
            let result = merge_worktree(&rt.spawner.base_repo, &meta).await;
            let (message, ok) = match result {
                Ok(()) => (format!("merged {}", meta.branch), true),
                Err(e) => (format!("merge failed: {e:#}"), false),
            };
            let _ = reduce(
                state,
                &AppEvent::OpDone {
                    record_id,
                    message,
                    ok,
                },
            );
        }
        Effect::Apply { record_id } => {
            let Some(meta) = rt.fleet.meta.get(&record_id).cloned() else {
                state.notice = Some("no worktree to apply from".into());
                return;
            };
            let result = apply_worktree(&rt.spawner.base_repo, &meta).await;
            let (message, ok) = match result {
                Ok(()) => (
                    "applied to the base working tree (uncommitted — you write the commit)"
                        .to_string(),
                    true,
                ),
                Err(e) => (format!("apply failed: {e:#}"), false),
            };
            let _ = reduce(
                state,
                &AppEvent::OpDone {
                    record_id,
                    message,
                    ok,
                },
            );
        }
    }
}

/// Diff-stat the worktree and run the verification checks; `None` when the
/// diff is empty (nothing to review).
async fn inspect_worktree(record_id: &str, meta: &WorkMeta, checks: &[String]) -> Option<Incoming> {
    let stat = crate::fleet_mcp::diff_stat(&meta.worktree, &meta.base_ref).await?;
    let files = stat["files"].as_u64().unwrap_or(0);
    if files == 0 {
        return None; // no work — stays idle, not review
    }
    // Per-worker verification gates: a failing check loops back to the agent.
    for check in checks {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(check)
            .current_dir(&meta.worktree)
            .output()
            .await;
        match out {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let mut text = format!(
                    "$ {check}
"
                );
                text.push_str(&String::from_utf8_lossy(&out.stdout));
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                // Keep the tail — that's where test failures summarize.
                if text.len() > 4000 {
                    text = format!("…{}", &text[text.len() - 4000..]);
                }
                return Some(Incoming::ChecksFailed {
                    record_id: record_id.to_string(),
                    output: text,
                });
            }
            Err(e) => {
                return Some(Incoming::ChecksFailed {
                    record_id: record_id.to_string(),
                    output: format!(
                        "$ {check}
failed to run: {e}"
                    ),
                });
            }
        }
    }
    Some(Incoming::ReviewReady {
        record_id: record_id.to_string(),
        files,
        adds: stat["adds"].as_u64().unwrap_or(0),
        dels: stat["dels"].as_u64().unwrap_or(0),
    })
}

/// Merge the agent's branch into the base repo, keeping history. Requires the
/// agent to have committed its work (a dirty worktree fails with guidance).
async fn merge_worktree(base_repo: &std::path::Path, meta: &WorkMeta) -> Result<()> {
    let dirty = crate::fleet_mcp::git_stdout(&meta.worktree, &["status", "--porcelain"]).await?;
    if !dirty.trim().is_empty() {
        anyhow::bail!(
            "the worktree has uncommitted changes — ask the agent to commit, or use p (apply)"
        );
    }
    let msg = format!("merge {}", meta.branch);
    crate::fleet_mcp::git_ok(base_repo, &["merge", "--no-ff", "-m", &msg, &meta.branch]).await
}

/// Apply the agent's diff onto the base working tree, uncommitted.
async fn apply_worktree(base_repo: &std::path::Path, meta: &WorkMeta) -> Result<()> {
    let patch =
        crate::fleet_mcp::git_stdout(&meta.worktree, &["diff", "--binary", &meta.base_ref]).await?;
    if patch.trim().is_empty() {
        anyhow::bail!("nothing to apply: the diff vs the spawn base is empty");
    }
    crate::fleet_mcp::git_apply(base_repo, &patch).await
}

/// Abort and await every in-flight prompt task (across all sessions) so their
/// `Arc<Session>` clones are released before teardown takes sole ownership.
async fn abort_prompt_tasks(tasks: &mut PromptTasks) {
    for (_, handles) in tasks.drain() {
        for handle in handles {
            handle.abort();
            // An aborted handle resolves to a JoinError; discarding is fine.
            let _ = handle.await;
        }
    }
}

/// Abort in-flight prompt tasks, drop pending permissions (defaulting them to
/// Deny in the substrate), and shut down every session. Called on every
/// loop-exit path.
async fn cleanup(rt: &mut Runtime<'_>) {
    abort_prompt_tasks(&mut rt.prompt_tasks).await;
    // Dropping a PendingPermission defaults it to Deny in the substrate.
    rt.pending.clear();
    for (_, sess) in rt.sessions.drain() {
        shutdown_session(sess).await;
    }
}

/// Best-effort session shutdown: take sole ownership and shut down; warn (don't
/// fail) if a clone is still outstanding or shutdown errors.
async fn shutdown_session(sess: Arc<Session>) {
    match Arc::try_unwrap(sess) {
        Ok(only) => {
            if let Err(e) = only.shutdown().await {
                tracing::warn!(error = %e, "session shutdown failed");
            }
        }
        Err(_) => {
            tracing::warn!("session still referenced at teardown; worktree may leak");
        }
    }
}

/// Extract the first structured diff from a permission's tool-call content,
/// ready for the TUI's line-diff renderer.
fn perm_diff(p: &bitrouter_substrate::up::PendingPermission) -> Option<DiffData> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    p.tool_call
        .fields
        .content
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find_map(|c| match c {
            ToolCallContent::Diff(d) => Some(DiffData {
                path: d.path.display().to_string(),
                old: d.old_text.clone().unwrap_or_default(),
                new: d.new_text.clone(),
            }),
            _ => None,
        })
}

/// Map a `PendingPermission`'s options to display data, matching `reduce_key`'s
/// y/a/n handling (allow-once / allow-always / deny).
fn perm_options(p: &bitrouter_substrate::up::PendingPermission) -> Vec<PermOption> {
    use agent_client_protocol::schema::v1::PermissionOptionKind;
    p.options
        .iter()
        .map(|o| {
            let (outcome, label) = match o.kind {
                PermissionOptionKind::AllowOnce => (PermissionOutcome::AllowOnce, "allow"),
                PermissionOptionKind::AllowAlways => {
                    (PermissionOutcome::AllowAlways, "allow always")
                }
                // Any reject/unknown kind maps to Deny — the reducer only offers y/a/n.
                _ => (PermissionOutcome::Deny, "deny"),
            };
            PermOption {
                outcome,
                label: label.to_string(),
            }
        })
        .collect()
}

fn quit_key() -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn alloc_port_takes_lowest_free_and_exhausts_cleanly() {
        let mut allocated = std::collections::HashMap::new();
        assert_eq!(super::alloc_port((3100, 3101), &allocated), Some(3100));
        allocated.insert("r0".to_string(), 3100);
        assert_eq!(super::alloc_port((3100, 3101), &allocated), Some(3101));
        allocated.insert("r1".to_string(), 3101);
        assert_eq!(
            super::alloc_port((3100, 3101), &allocated),
            None,
            "an exhausted pool allocates nothing rather than colliding"
        );
        // Closing an agent frees its port for the next spawn.
        allocated.remove("r0");
        assert_eq!(super::alloc_port((3100, 3101), &allocated), Some(3100));
    }

    #[tokio::test]
    async fn probe_serve_ok_when_listening() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.to_string();
        assert!(super::probe_serve(&addr).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn probe_serve_warns_when_port_closed() -> anyhow::Result<()> {
        // Bind then drop to get a local port that is very likely closed.
        let addr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            listener.local_addr()?.to_string()
        };
        let warning = super::probe_serve(&addr).await;
        assert!(
            warning.is_some_and(|w| w.contains("bitrouter serve")),
            "closed port must produce an actionable warning"
        );
        Ok(())
    }

    static RESTORED: AtomicBool = AtomicBool::new(false);

    fn mark_restored() {
        RESTORED.store(true, Ordering::SeqCst);
    }

    /// Fault-injection: a panic on any thread must run the chained restore
    /// (terminal un-rawed) before the previous hook prints the message. The
    /// injected restore records instead of touching the real terminal.
    #[test]
    fn panic_hook_restores_terminal_before_reporting() {
        super::install_panic_restore(mark_restored);
        let joined = std::thread::spawn(|| {
            // Deliberate fault injection — the behavior under test is the
            // panic hook itself, so the thread must actually panic.
            std::panic::panic_any("injected tui fault");
        })
        .join();
        assert!(joined.is_err(), "injected fault should panic the thread");
        assert!(
            RESTORED.load(Ordering::SeqCst),
            "panic hook must run the terminal restore"
        );
    }
}
