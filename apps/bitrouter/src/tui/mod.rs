//! `bitrouter tui` — in-process multi-agent manager.
mod event;
mod highlight;
mod notify;
mod pty;
mod pump;
mod state;
mod term;
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

/// Launch the TUI. `--agent` names either a native harness (`claude`,
/// `codex`, `opencode`, `pi`) — the **orchestrator**, hosted at full
/// fidelity in a PTY pane (TUI_SPEC §2/§3) with the fleet MCP bridge
/// injected where the harness supports MCP — or a configured `agents:`
/// entry, which falls back to an ACP-rendered primary pane. `--model` pins
/// the orchestrator's model (a daemon-routable id). `n` (in AGENT mode)
/// spawns worktree-isolated ACP subagents either way.
pub async fn run(agent_id: &str, worktree: Option<&str>, model: Option<&str>) -> Result<()> {
    // ── Config + catalog. ──
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    let catalog = bitrouter_sdk::acp::ConfigAcpRoutingTable::from_configs(
        cfg.agents.iter().map(|(k, v)| (k.clone(), v.clone())),
    )
    .context("building acp catalog from config.agents")?;
    let agent_ids: Vec<String> = cfg.agents.keys().cloned().collect();
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let ports = (cfg.worktrees.ports.from, cfg.worktrees.ports.to);

    // ── Channel. The loop keeps `tx` to pump agents spawned later. ──
    let (tx, rx) = unbounded_channel::<Incoming>();
    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    let mut port_leases: crate::fleet::PortLeases = HashMap::new();
    let mut ptys: HashMap<String, pty::PtyPane> = HashMap::new();

    // The manager's durable memory (`.bitrouter/fleet-state.json`) and the
    // self-ignoring state dir it lives in.
    let _ = bitrouter_substrate::dotdir::ensure_self_ignored(&base_repo.join(".bitrouter"));
    let fleet_store = bitrouter_substrate::fleet::FleetStore::new(&base_repo);
    let previous_fleet = fleet_store.load().await;

    // The fleet socket (Unix): MCP bridge subprocesses connect back through
    // it so orchestrator-spawned subagents appear in the rail and their
    // gated permissions reach the human's decision queue (TUI_SPEC §2/§5).
    #[cfg(unix)]
    let fleet_sock = start_fleet_listener(&base_repo, tx.clone());
    #[cfg(unix)]
    let fleet_sock_path = fleet_sock.as_ref().map(|s| s.0.clone());
    #[cfg(not(unix))]
    let fleet_sock_path: Option<std::path::PathBuf> = None;

    // The daemon's advertised model ids (best-effort): fills the synthesized
    // provider catalogs for the config-routed orchestrators (opencode, pi).
    let models = fetch_models(&cfg.server.listen).await;

    // `--agent` names the orchestrator by its interactive binary (`claude`,
    // `agy`, …) or by its catalog id (`antigravity`) — but a configured
    // `agents:` entry with the same name keeps meaning the ACP pane.
    let orchestrator = crate::harness::by_interactive_binary(agent_id).or_else(|| {
        if cfg.agents.contains_key(agent_id) {
            None
        } else {
            crate::harness::by_id(agent_id).filter(|h| h.interactive_binary.is_some())
        }
    });
    let base_url = crate::spawn::derive_base_url(&cfg.server.listen);
    let initial_pane = if let Some(h) = orchestrator {
        // ── Orchestrator: the native harness TUI in a PTY pane. ──
        if worktree.is_some() {
            anyhow::bail!(
                "--worktree applies to ACP agents; the orchestrator runs in the base repo"
            );
        }
        let (record_id, pane, pty_pane) = launch_orchestrator(
            h,
            &OrchestratorCtx {
                base_url: &base_url,
                base_repo: &base_repo,
                model,
                models: &models,
                fleet_sock: fleet_sock_path.as_deref(),
            },
            tx.clone(),
            "orchestrator",
        )?;
        ptys.insert(record_id, pty_pane);
        pane
    } else if cfg.agents.contains_key(agent_id) {
        // ── ACP fallback: the primary agent rendered from typed events.
        // Runs in the base repo (worktree strictly opt-in via --worktree)
        // but still draws a fleet PORT. Worktrees are retained on close;
        // transcripts stay on (LaunchOptions default). ──
        let initial_lease = crate::fleet::reserve_port(&base_repo, ports);
        let initial_port = initial_lease.as_ref().map(|l| l.port());
        let options = bitrouter_substrate::engine::LaunchOptions {
            worktree: worktree.map(|name| bitrouter_substrate::worktree::WorktreeSpec {
                name: name.to_string(),
                branch: None,
                remove_on_shutdown: false,
            }),
            env: crate::fleet::port_env(initial_port),
            ..Default::default()
        };
        let session = Session::launch(&catalog, agent_id, base_repo.clone(), options)
            .await
            .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
        let record_id = session.state().record_id.clone();
        let session = Arc::new(session);
        pump::spawn(Arc::clone(&session), record_id.clone(), tx.clone());
        sessions.insert(record_id.clone(), session);
        if let Some(lease) = initial_lease {
            port_leases.insert(record_id.clone(), lease);
        }
        let mut pane = PaneState::new(record_id, agent_id.to_string());
        pane.port = initial_port;
        pane
    } else {
        // Fail a bad --agent id up front with the fix in the message.
        let mut available: Vec<String> = crate::harness::CATALOG
            .iter()
            .filter_map(|h| h.interactive_binary.map(str::to_string))
            .collect();
        available.extend(agent_ids.iter().cloned());
        available.sort();
        available.dedup();
        anyhow::bail!(
            "'{agent_id}' is neither a native harness nor an `agents:` entry — available: {}",
            if available.is_empty() {
                "none configured (add one with `bitrouter agents install <id>`)".to_string()
            } else {
                available.join(", ")
            }
        );
    };

    // ── Initial state. ──
    let mut state = AppState::new(initial_pane);
    state.bootstrap_cmd = cfg.worktrees.bootstrap.clone();
    state.set_available_agents(agent_ids);
    // The `new session` picker offers every harness with an interactive
    // binary — the same set `--agent` accepts.
    state.available_sessions = crate::harness::CATALOG
        .iter()
        .filter_map(|h| h.interactive_binary.map(str::to_string))
        .collect();
    state.set_harness_map(
        cfg.agents
            .iter()
            .map(|(k, v)| (k.clone(), harness_tag(&v.transport)))
            .collect(),
    );
    // Agents route LLM traffic through `bitrouter serve`; probe it once so a
    // missing proxy is an up-front notice instead of silent agent failures.
    // The loop re-probes every few seconds to keep the `serve` dot live.
    match probe_serve(&cfg.server.listen).await {
        Some(warning) => {
            state.serve_ok = Some(false);
            state.notice = Some(warning);
        }
        None => state.serve_ok = Some(true),
    }
    // https://no-color.org: any non-empty value disables foreground colors.
    state.no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    // Surface the previous fleet's memory — the daemon warning wins the
    // single notice slot when both apply.
    if state.notice.is_none()
        && let Some(notice) = previous_fleet.as_ref().and_then(previous_fleet_notice)
    {
        state.notice = Some(notice);
    }

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
            gateway_base: base_url,
            models,
            model: model.map(str::to_string),
            fleet_sock: fleet_sock_path,
            tx,
        },
        fleet: Fleet {
            ports,
            leases: port_leases,
            meta: HashMap::new(),
            checks: cfg.worktrees.checks.clone(),
        },
        #[cfg(unix)]
        bridges: HashMap::new(),
        #[cfg(unix)]
        bridge_agents: HashMap::new(),
        #[cfg(unix)]
        bridge_perms: HashMap::new(),
        #[cfg(unix)]
        _fleet_sock: fleet_sock,
        ptys,
        notify: notify::NotifyPath::detect(),
        fleet_store,
        session_seq: 0,
        listen: cfg.server.listen.clone(),
        last_fleet: None,
    };
    let result = event_loop(&mut terminal, state, rx, rt).await;
    restore_terminal();
    result
}

/// Fleet-level resources the loop allocates per subagent (TUI_SPEC §6).
struct Fleet {
    /// The inclusive `PORT` pool (`worktrees.ports`).
    ports: (u16, u16),
    /// record_id → cross-process port lease; dropping frees the port.
    leases: crate::fleet::PortLeases,
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

/// Terse harness tag for a pane header / roster meta line. Invocation
/// matching against the catalog names the harness even through a runner
/// (`npx …claude-code-acp…` → `claude`, not `npx`); a non-catalog agent
/// falls back to the command basename (`/usr/local/bin/foo` → `foo`).
fn harness_tag(transport: &bitrouter_sdk::acp::AcpTransport) -> String {
    match transport {
        bitrouter_sdk::acp::AcpTransport::Stdio { command, args, .. } => {
            if let Some(h) = crate::harness::match_invocation(command, args) {
                return h.interactive_binary.unwrap_or(h.id).to_string();
            }
            std::path::Path::new(command)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| command.clone())
        }
    }
}

/// Attach (TUI_SPEC §13-B4): relaunch the ACP agent's harness interactively
/// on a PTY in its worktree — native fidelity for driving one agent — and
/// resume the same provider-native conversation when its id is known.
/// Detach (`Ctrl-A x` on the attach pane) kills only the interactive child;
/// the ACP session is untouched.
fn attach_interactive(record_id: &str, state: &mut AppState, rt: &mut Runtime<'_>) {
    let attach_id = format!("attach:{record_id}");
    if rt.ptys.contains_key(&attach_id) {
        state.notice = Some("already attached — Ctrl-A x on the attach pane detaches".into());
        return;
    }
    let Some(sess) = rt.sessions.get(record_id) else {
        state.notice = Some("nothing to attach to".into());
        return;
    };
    let agent_id = sess.state().agent_id.clone();
    // Map the ACP agent's configured invocation back to its catalog harness
    // (invocation matching — the YAML key carries no semantics).
    let Some(bitrouter_sdk::acp::AcpTransport::Stdio { command, args, .. }) =
        rt.spawner.catalog.lookup(&agent_id)
    else {
        state.notice = Some(format!("no transport for '{agent_id}'"));
        return;
    };
    let Some(h) = crate::harness::match_invocation(command, args) else {
        state.notice = Some(format!(
            "'{agent_id}' matches no catalog harness — cannot attach"
        ));
        return;
    };
    let Some(binary) = h.interactive_binary else {
        state.notice = Some(format!("'{}' has no interactive binary to attach", h.id));
        return;
    };
    // Drive the SAME work: cwd = the agent's worktree (base repo when none).
    let cwd = rt
        .fleet
        .meta
        .get(record_id)
        .map(|m| m.worktree.clone())
        .or_else(|| sess.worktree_path().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| rt.spawner.base_repo.clone());
    // Continue the SAME conversation when the provider-native session id is
    // known (claude-code-acp reports it as agentSessionId).
    let mut extra: Vec<String> = Vec::new();
    if let Some(sid) = &sess.state().agent_session_id {
        match binary {
            "claude" => extra.extend(["--resume".to_string(), sid.clone()]),
            "codex" => extra.extend(["resume".to_string(), sid.clone()]),
            _ => {}
        }
    }
    let auth = crate::harness::resolve_gateway_auth(
        std::env::var(crate::harness::BITROUTER_API_KEY_ENV).ok(),
        false,
    )
    .unwrap_or_else(|| crate::harness::PLACEHOLDER_API_KEY.to_string());
    // Same overlay family as the orchestrator (synthesizes config for
    // opencode/pi), but no MCP bridge — an attach drives one agent, it
    // doesn't orchestrate. Synth files live under the BASE repo's
    // .bitrouter (never the agent's worktree, where they would dirty the
    // review diff), in a per-attach dir so attaches don't clobber the
    // orchestrator's own synth config.
    let state_dir = rt
        .spawner
        .base_repo
        .join(".bitrouter")
        .join(format!("attach-{record_id}"));
    let overlay = match h.orchestrator_overlay(
        &rt.spawner.gateway_base,
        &auth,
        None,
        &rt.spawner.models,
        None,
        &state_dir,
    ) {
        Ok(o) => o,
        Err(e) => {
            state.notice = Some(format!("attach overlay failed: {e:#}"));
            return;
        }
    };
    let mut all_args = overlay.args.clone();
    all_args.extend(extra);
    match pty::PtyPane::spawn(
        &attach_id,
        &pty::PtyLaunch {
            command: binary,
            args: &all_args,
            env: &overlay.env,
            cwd: &cwd,
        },
        80,
        24,
        rt.spawner.tx.clone(),
    ) {
        Ok(pane) => {
            rt.ptys.insert(attach_id.clone(), pane);
            let _ = reduce(
                state,
                &AppEvent::PtyAttached {
                    record_id: attach_id,
                    agent_id: format!("{binary}⤴{}", agent_id),
                },
            );
        }
        Err(e) => state.notice = Some(format!("attach failed: {e:#}")),
    }
}

/// Everything a `launch_orchestrator` call needs beyond the harness and the
/// pane's record id.
struct OrchestratorCtx<'a> {
    base_url: &'a str,
    base_repo: &'a std::path::Path,
    model: Option<&'a str>,
    models: &'a [String],
    fleet_sock: Option<&'a std::path::Path>,
}

/// Launch the orchestrator: the harness's interactive binary on a PTY, its
/// LLM traffic routed through the daemon (same overlay as `bitrouter launch`)
/// and the fleet MCP bridge injected — where the harness has an MCP
/// mechanism — so it can spawn/manage subagents (TUI_SPEC §2's data flow).
fn launch_orchestrator(
    h: &'static crate::harness::Harness,
    ctx: &OrchestratorCtx<'_>,
    tx: UnboundedSender<Incoming>,
    record_id: &str,
) -> Result<(String, PaneState, pty::PtyPane)> {
    let OrchestratorCtx {
        base_url,
        base_repo,
        model,
        models,
        fleet_sock,
    } = *ctx;
    let Some(binary) = h.interactive_binary else {
        anyhow::bail!("harness '{}' has no interactive binary", h.id);
    };
    let auth = crate::harness::resolve_gateway_auth(
        std::env::var(crate::harness::BITROUTER_API_KEY_ENV).ok(),
        false,
    )
    .unwrap_or_else(|| crate::harness::PLACEHOLDER_API_KEY.to_string());
    // Synth-config dir: the initial session keeps the historic `.bitrouter`
    // home; later ones get their own subdir so same-harness sessions never
    // clobber each other's synthesized files.
    let state_dir = if record_id == "orchestrator" {
        base_repo.join(".bitrouter")
    } else {
        base_repo.join(".bitrouter").join(record_id)
    };
    let overlay = h
        .orchestrator_overlay(
            base_url,
            &auth,
            model,
            models,
            Some(&fleet_mcp_server()),
            &state_dir,
        )
        .with_context(|| format!("assembling the '{}' orchestrator overlay", h.id))?;
    let args = overlay.args.clone();
    // The fleet-socket path rides the PTY env: the harness inherits it, and
    // so does the `bitrouter mcp serve --backend fleet` subprocess it spawns,
    // which connects back through it.
    let mut env = overlay.env.clone();
    if let Some(sock) = fleet_sock {
        env.push((
            crate::fleet::TUI_SOCK_ENV.to_string(),
            sock.display().to_string(),
        ));
    }

    // Sized properly on the first draw (the pane rect is recorded and the
    // loop resizes + SIGWINCHes the child).
    let pane = pty::PtyPane::spawn(
        record_id,
        &pty::PtyLaunch {
            command: binary,
            args: &args,
            env: &env,
            cwd: base_repo,
        },
        80,
        24,
        tx,
    )
    .with_context(|| format!("launching orchestrator '{binary}' on a pty"))?;

    let mut pane_state = PaneState::new(record_id.to_string(), binary.to_string());
    pane_state.kind = crate::tui::state::PaneKind::Pty;
    pane_state.harness = "pty".to_string();
    pane_state.model = model.map(str::to_string);
    Ok((record_id.to_string(), pane_state, pane))
}

/// Removes the fleet socket file when the TUI exits (any path — Drop).
#[cfg(unix)]
struct SockCleanup(std::path::PathBuf);

#[cfg(unix)]
impl Drop for SockCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bind the per-repo fleet socket and pump bridge connections into the
/// loop's channel: each accepted connection surfaces as `BridgeConnected`
/// (carrying the write half), each NDJSON line as `Incoming::Bridge`, EOF as
/// `BridgeGone`. `None` (with a log line) when binding fails — the TUI still
/// runs; bridges then fall back to their headless auto-policy.
#[cfg(unix)]
fn start_fleet_listener(
    base_repo: &std::path::Path,
    tx: UnboundedSender<Incoming>,
) -> Option<SockCleanup> {
    let path = crate::fleet::tui_sock_path(base_repo);
    // A previous unclean exit leaves the file; the bind below re-creates it.
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "fleet socket bind failed");
            return None;
        }
    };
    tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            seq += 1;
            let conn = seq;
            let (read, write) = stream.into_split();
            if tx
                .send(Incoming::BridgeConnected {
                    conn,
                    writer: write,
                })
                .is_err()
            {
                break; // loop gone — stop accepting
            }
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match serde_json::from_str::<crate::fleet::BridgeMsg>(&line) {
                        Ok(msg) => {
                            if tx.send(Incoming::Bridge { conn, msg }).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "unparseable fleet bridge message")
                        }
                    }
                }
                let _ = tx.send(Incoming::BridgeGone { conn });
            });
        }
    });
    Some(SockCleanup(path))
}

/// Write one NDJSON `TuiMsg` to a bridge connection (best-effort — a dead
/// bridge's messages are moot).
#[cfg(unix)]
async fn bridge_write(writer: &mut tokio::net::unix::OwnedWriteHalf, msg: &crate::fleet::TuiMsg) {
    use tokio::io::AsyncWriteExt;
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes()).await;
    }
}

/// The fleet MCP bridge as a stdio server spec: this binary running
/// `mcp serve --backend fleet`, so the orchestrator's `spawn_subagent`/…
/// tools reach this repo's fleet (stdio subprocess, per TUI_SPEC §15-Q2).
fn fleet_mcp_server() -> crate::harness::McpServer {
    crate::harness::McpServer {
        name: "bitrouter_fleet".to_string(),
        command: std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "bitrouter".to_string()),
        args: ["mcp", "serve", "--backend", "fleet"]
            .map(str::to_string)
            .to_vec(),
    }
}

/// Best-effort fetch of the daemon's advertised model ids (`/v1/models`) —
/// they fill the synthesized provider catalogs for the config-routed
/// orchestrators. Empty when the daemon is unreachable (the TUI still
/// launches; the startup notice already covers the daemon being down).
async fn fetch_models(listen: &str) -> Vec<String> {
    let base = crate::spawn::derive_base_url(listen);
    let request = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .timeout(std::time::Duration::from_secs(2))
        .send();
    let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(3), request).await
    else {
        return Vec::new();
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

/// Whether the kitty keyboard enhancement was pushed (so restore only pops
/// what was pushed — a stray pop confuses terminals without the protocol).
static KITTY_PUSHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    // Any failure past this point returns `Err` (the panic hook never fires),
    // so undo raw mode here or the user's shell is left with no echo.
    match setup_after_raw() {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            let _ = disable_raw_mode();
            Err(e)
        }
    }
}

fn setup_after_raw() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        // Focus events drive away-notifications and the seen/unseen decay.
        crossterm::event::EnableFocusChange
    )?;
    // Save the user's title so restore can put it back (XTWINOPS push/pop);
    // the loop then uses the title as the attention badge.
    write_out(&notify::NotifyPath::detect().title_push());
    // Kitty keyboard enhancement: puts the manager leader in a keyspace no
    // legacy binding collides with and makes Shift-Enter distinct from Enter
    // (the composer's newline). Only where the terminal supports it.
    if matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) && execute!(
        out,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    )
    .is_ok()
    {
        KITTY_PUSHED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Best-effort terminal restore (raw mode off, leave alt-screen, show cursor).
/// Needs no `Terminal` handle so the panic hook can call it from any thread;
/// errors are ignored — by then we're exiting anyway.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    if KITTY_PUSHED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = execute!(out, crossterm::event::PopKeyboardEnhancementFlags);
    }
    write_out(&notify::NotifyPath::detect().title_pop());
    let _ = execute!(
        out,
        crossterm::event::DisableFocusChange,
        crossterm::event::DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Write a raw escape sequence to the outer terminal (best-effort — by the
/// time a write fails we're exiting anyway).
fn write_out(bytes: &[u8]) {
    use std::io::Write;
    let mut out = io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
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
    /// The daemon gateway base URL (derived from `server.listen`) — routing
    /// overlay for interactive attaches.
    gateway_base: String,
    /// The daemon's advertised model ids at startup (may be empty) — fills
    /// synthesized provider catalogs on attach.
    models: Vec<String>,
    /// The `--model` pin the manager launched with — new sessions inherit it.
    model: Option<String>,
    /// The fleet socket path, injected into every orchestrator PTY's env so
    /// its MCP bridge subprocess can connect back (Unix; `None` elsewhere or
    /// when binding failed).
    fleet_sock: Option<std::path::PathBuf>,
    tx: UnboundedSender<Incoming>,
}

/// In-flight prompt tasks keyed by the owning session's `record_id`, so a
/// pane close can abort+await just that session's tasks.
type PromptTasks = HashMap<String, Vec<tokio::task::JoinHandle<()>>>;

/// The live session registry plus everything else `apply_effect` mutates,
/// bundled so the function stays under clippy's `too_many_arguments` threshold.
struct Runtime<'a> {
    sessions: HashMap<String, Arc<Session>>,
    /// Per-session FIFO of unresolved permission requests. Only the front is
    /// on screen; the rest wait their turn — a second request arriving while
    /// one is displayed must queue, never silently deny the first.
    pending:
        HashMap<String, std::collections::VecDeque<bitrouter_substrate::up::PendingPermission>>,
    prompt_tasks: PromptTasks,
    spawner: Spawner<'a>,
    fleet: Fleet,
    /// Fleet-socket state (Unix): connected MCP bridges by connection id,
    /// their mirror panes, and their in-flight permission requests
    /// (`record_id → (conn, bridge-local id)`).
    #[cfg(unix)]
    bridges: HashMap<u64, tokio::net::unix::OwnedWriteHalf>,
    #[cfg(unix)]
    bridge_agents: HashMap<String, u64>,
    #[cfg(unix)]
    bridge_perms: HashMap<String, (u64, u64)>,
    /// Removes the fleet socket file at drop (Unix).
    #[cfg(unix)]
    _fleet_sock: Option<SockCleanup>,
    /// Live PTY panes by record id: the orchestrator (when `--agent` named a
    /// native harness) plus any interactive attaches (TUI_SPEC §13-B4).
    ptys: HashMap<String, pty::PtyPane>,
    /// The host terminal's notification dialect, detected once at startup.
    notify: notify::NotifyPath,
    /// Durable fleet-state writer (`.bitrouter/fleet-state.json`) — the
    /// manager's memory across stops. Written on durable changes and once,
    /// with `clean_shutdown`, at teardown.
    fleet_store: bitrouter_substrate::fleet::FleetStore,
    /// Monotonic counter minting record ids for `new session` panes.
    session_seq: u64,
    /// The daemon's configured listen address, for the periodic liveness
    /// probe behind the status bar's `serve` dot.
    listen: String,
    /// The last snapshot written (unstamped), for change detection.
    last_fleet: Option<bitrouter_substrate::fleet::FleetState>,
}

/// Build the (unstamped) durable snapshot of the manager's current state.
fn fleet_state(state: &AppState) -> bitrouter_substrate::fleet::FleetState {
    bitrouter_substrate::fleet::FleetState {
        version: bitrouter_substrate::fleet::FLEET_STATE_VERSION,
        saved_at: 0, // stamped by FleetStore::save
        clean_shutdown: false,
        writer_pid: std::process::id(),
        sessions: state.fleet_sessions(),
        agents: state.fleet_agents(),
    }
}

/// Persist the current fleet state if it changed since the last write.
/// Best-effort: durability must never take the live manager down.
async fn flush_fleet_state(state: &AppState, rt: &mut Runtime<'_>) {
    let snapshot = fleet_state(state);
    if rt.last_fleet.as_ref() == Some(&snapshot) {
        return;
    }
    if let Err(e) = rt.fleet_store.save(&snapshot).await {
        tracing::warn!(error = %e, "failed to write fleet state");
    }
    rt.last_fleet = Some(snapshot);
}

/// The teardown write: same snapshot, marked as an orderly stop.
async fn flush_fleet_state_clean(state: &AppState, rt: &mut Runtime<'_>) {
    let mut snapshot = fleet_state(state);
    snapshot.clean_shutdown = true;
    if let Err(e) = rt.fleet_store.save(&snapshot).await {
        tracing::warn!(error = %e, "failed to write final fleet state");
    }
}

/// One-line startup notice about the previous fleet, or `None` when there is
/// nothing worth surfacing (no file, or it held no agents).
fn previous_fleet_notice(prev: &bitrouter_substrate::fleet::FleetState) -> Option<String> {
    if prev.agents.is_empty() {
        return None;
    }
    let reviews = prev.agents.iter().filter(|a| a.review.is_some()).count();
    let drafts = prev.agents.iter().filter(|a| a.draft.is_some()).count();
    let mut notice = format!("previous fleet remembered: {} agent(s)", prev.agents.len());
    if prev.sessions.len() > 1 {
        notice.push_str(&format!(", {} sessions", prev.sessions.len()));
    }
    if reviews > 0 {
        notice.push_str(&format!(", {reviews} mid-review"));
    }
    if drafts > 0 {
        notice.push_str(&format!(", {drafts} unsent draft(s)"));
    }
    if !prev.clean_shutdown {
        notice.push_str(" · unclean shutdown");
    }
    notice.push_str(" — .bitrouter/fleet-state.json");
    Some(notice)
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
    // The terminal title doubles as the attention badge; re-emit on change.
    let mut last_title = String::new();

    loop {
        let badge = state.title_badge();
        if badge != last_title {
            write_out(&rt.notify.title(&badge));
            last_title = badge;
        }
        // Snapshot the PTY grids for this frame (the emulators live
        // loop-side; state stays pure).
        let pty_views: Vec<ui::PtyView> = rt
            .ptys
            .iter()
            .map(|(rid, pane)| ui::PtyView {
                record_id: rid.clone(),
                lines: pane.backend.lines(state.no_color),
            })
            .collect();
        if let Err(e) = terminal.draw(|f| ui::render(&mut state, &pty_views, f)) {
            flush_fleet_state_clean(&state, &mut rt).await;
            cleanup(&mut rt).await;
            return Err(e).context("draw");
        }
        // Resize recovery (§9): the drawn pane rect is authoritative — resize
        // the emulator and SIGWINCH the child whenever it changes.
        for (rid, (cols, rows)) in &state.pty_areas {
            if let Some(pane) = rt.ptys.get_mut(rid) {
                pane.resize(*cols, *rows);
            }
        }
        if state.should_quit {
            // The manager's memory of the fleet as it stood at the stop —
            // written before teardown denies pendings and kills children.
            flush_fleet_state_clean(&state, &mut rt).await;
            cleanup(&mut rt).await;
            return Ok(());
        }

        // Convert the next input/pumped item into a pure AppEvent.
        let app_event: Option<AppEvent> = tokio::select! {
            maybe_key = keys.next() => match maybe_key {
                // Windows delivers a Release event for every keystroke (and
                // kitty terminals can too); only Press/Repeat may reach the
                // reducer or every key fires twice.
                Some(Ok(CtEvent::Key(k)))
                    if k.kind == crossterm::event::KeyEventKind::Release => None,
                Some(Ok(CtEvent::Key(k))) => Some(AppEvent::Key(k)),
                // Wheel scroll pages the focused pane's scrollback.
                Some(Ok(CtEvent::Mouse(m))) => match m.kind {
                    crossterm::event::MouseEventKind::ScrollUp =>
                        Some(AppEvent::Scroll { up: true }),
                    crossterm::event::MouseEventKind::ScrollDown =>
                        Some(AppEvent::Scroll { up: false }),
                    _ => None,
                },
                // Focus drives away-notifications; regaining it marks the
                // shown panes seen (the done → idle decay).
                Some(Ok(CtEvent::FocusGained)) => Some(AppEvent::Focus(true)),
                Some(Ok(CtEvent::FocusLost)) => Some(AppEvent::Focus(false)),
                // Resize needs no state change: `None` continues to the top of
                // the loop, whose draw autoresizes to the new size.
                Some(Ok(_)) => None,
                Some(Err(_)) | None => Some(AppEvent::ForceQuit),
            },
            _ = ticker.tick() => Some(AppEvent::Tick),
            maybe_in = rx.recv() => match maybe_in {
                Some(incoming) => convert_incoming(incoming, &mut rt),
                None => Some(AppEvent::ForceQuit),
            },
        };

        if let Some(app_event) = app_event {
            let is_tick = matches!(app_event, AppEvent::Tick);
            let effects = reduce(&mut state, &app_event);
            for effect in effects {
                apply_effect(effect, &mut state, &mut rt).await;
            }
            // Durability heartbeat: at most once a second (every 5th tick),
            // and only when the snapshot content actually changed — a crash
            // loses at most the last second of manager state.
            if is_tick && state.tick.is_multiple_of(5) {
                flush_fleet_state(&state, &mut rt).await;
            }
            // Liveness probe for the status bar's `serve` dot (~every 5s):
            // a daemon that dies mid-session must not fail silently until
            // the next agent call errors.
            if is_tick && state.tick.is_multiple_of(25) {
                let listen = rt.listen.clone();
                let tx = rt.spawner.tx.clone();
                tokio::spawn(async move {
                    let ok = probe_serve(&listen).await.is_none();
                    let _ = tx.send(Incoming::ServeStatus { ok });
                });
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
                // Queue behind any request already on screen — resolving the
                // front surfaces the next (`Incoming::PermissionNext`).
                let queue = rt.pending.entry(record_id.clone()).or_default();
                queue.push_back(*p);
                if queue.len() > 1 {
                    return None;
                }
                let front = queue.front()?;
                Some(perm_event(&record_id, front, &rt.spawner.base_repo))
            } else {
                // Pane already closed; dropping `p` denies the request.
                None
            }
        }
        Incoming::PermissionNext { record_id } => {
            let front = rt.pending.get(&record_id)?.front()?;
            Some(perm_event(&record_id, front, &rt.spawner.base_repo))
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
        Incoming::PtyOutput { record_id, bytes } => {
            if let Some(pane) = rt.ptys.get_mut(&record_id) {
                // Feed the emulator; re-emit OSC-52 clipboard writes verbatim
                // to the outer terminal (tmux allow-passthrough pattern) so
                // copy-from-the-inner-app reaches the user's clipboard.
                for seq in pane.feed(&bytes) {
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let _ = out.write_all(&seq);
                    let _ = out.flush();
                }
            }
            None // the next draw renders the updated grid
        }
        Incoming::PtyExited { record_id } => Some(AppEvent::Exited { record_id }),
        Incoming::ServeStatus { ok } => Some(AppEvent::ServeStatus { ok }),
        #[cfg(unix)]
        Incoming::BridgeConnected { conn, writer } => {
            rt.bridges.insert(conn, writer);
            Some(AppEvent::BridgeConnected { conn })
        }
        #[cfg(unix)]
        Incoming::Bridge { conn, msg } => match msg {
            crate::fleet::BridgeMsg::Spawned {
                handle,
                agent,
                port,
            } => {
                let record_id = format!("mcp:{handle}");
                rt.bridge_agents.insert(record_id.clone(), conn);
                Some(AppEvent::BridgeSpawned {
                    record_id,
                    agent_id: agent,
                    port,
                })
            }
            crate::fleet::BridgeMsg::State { handle, state } => Some(AppEvent::BridgeState {
                record_id: format!("mcp:{handle}"),
                state,
            }),
            crate::fleet::BridgeMsg::Closed { handle } => {
                let record_id = format!("mcp:{handle}");
                rt.bridge_agents.remove(&record_id);
                rt.bridge_perms.remove(&record_id);
                Some(AppEvent::Exited { record_id })
            }
            crate::fleet::BridgeMsg::Permission {
                id,
                handle,
                title,
                diff,
                options,
            } => {
                let record_id = format!("mcp:{handle}");
                rt.bridge_perms.insert(record_id.clone(), (conn, id));
                Some(AppEvent::Permission {
                    record_id,
                    title,
                    diff: diff.map(|d| DiffData {
                        path: d.path,
                        old: d.old,
                        new: d.new,
                    }),
                    options: options
                        .into_iter()
                        .map(|o| PermOption {
                            outcome: crate::fleet::outcome_from_str(&o.outcome),
                            label: o.label,
                        })
                        .collect(),
                    // The bridge auto-allows low risk itself; only gated
                    // (high-risk) requests reach the human.
                    risk: crate::risk::Risk::High,
                })
            }
        },
        #[cfg(unix)]
        Incoming::BridgeGone { conn } => {
            rt.bridges.remove(&conn);
            let record_ids: Vec<String> = rt
                .bridge_agents
                .iter()
                .filter(|(_, c)| **c == conn)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &record_ids {
                rt.bridge_agents.remove(id);
                rt.bridge_perms.remove(id);
            }
            Some(AppEvent::BridgeGone { record_ids })
        }
    }
}

/// Apply one reducer effect against the live session registry: send prompts,
/// resolve permissions, and (async) spawn/close sessions.
async fn apply_effect(effect: Effect, state: &mut AppState, rt: &mut Runtime<'_>) {
    match effect {
        Effect::Quit => {}
        Effect::Bell => {
            // Ring the terminal bell (BEL). Best-effort; ignore write errors.
            write_out(b"\x07");
        }
        Effect::Notify { title, body } => {
            // The human is away — deliver through the host terminal's
            // notification escape (it decides how to show it).
            if let Some(seq) = rt.notify.notification(&title, &body) {
                write_out(&seq);
            }
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
            let mut emptied = false;
            if let Some(queue) = rt.pending.get_mut(&record_id) {
                if let Some(p) = queue.pop_front() {
                    // Map the reducer's y/a/n outcome onto the exact option
                    // the upstream offered (validated `optionId`, per ACP).
                    let selected =
                        bitrouter_substrate::translate::select_option(outcome, &p.options);
                    p.resolve(selected);
                }
                emptied = queue.is_empty();
                if !emptied {
                    // Another request queued while this one was on screen —
                    // route it through the channel so it surfaces as a fresh
                    // display event on the next loop pass.
                    let _ = rt.spawner.tx.send(Incoming::PermissionNext {
                        record_id: record_id.clone(),
                    });
                }
            }
            if emptied {
                rt.pending.remove(&record_id);
            }
            // A bridge subagent's pending resolves over the fleet socket —
            // the owning bridge holds the real handle.
            #[cfg(unix)]
            if let Some((conn, id)) = rt.bridge_perms.remove(&record_id)
                && let Some(writer) = rt.bridges.get_mut(&conn)
            {
                bridge_write(
                    writer,
                    &crate::fleet::TuiMsg::Resolve {
                        id,
                        outcome: crate::fleet::outcome_str(outcome).to_string(),
                    },
                )
                .await;
            }
        }
        Effect::SpawnAgent { agent_id } => {
            // Fleet-managed subagents are worktree-isolated BY DEFAULT
            // (TUI_SPEC §6): each gets its own worktree + branch
            // (`bitrouter/<agent>-<record16>`, based on the manager's HEAD),
            // a PORT from the pool, and — when the human approved it — the
            // bootstrap hook. Worktrees are retained on close: cleanup is
            // gated on merged-or-discarded, never automatic.
            let tag = crate::fleet::branch_tag(&agent_id);
            let lease = crate::fleet::reserve_port(&rt.spawner.base_repo, rt.fleet.ports);
            let port = lease.as_ref().map(|l| l.port());
            let options = bitrouter_substrate::engine::LaunchOptions {
                worktree: Some(crate::fleet::worktree_spec(&tag)),
                worktree_bootstrap: match state.bootstrap_decision {
                    Some(true) => state.bootstrap_cmd.clone(),
                    _ => None,
                },
                env: crate::fleet::port_env(port),
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
                        let handle = crate::fleet::record16(&rid);
                        let base_ref = crate::fleet::base_head(&rt.spawner.base_repo).await;
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
                    if let Some(lease) = lease {
                        rt.fleet.leases.insert(rid.clone(), lease);
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
                    // The full error chain goes to the log; the reducer
                    // flattens it into the one-line mode-bar notice.
                    tracing::warn!(agent = %agent_id, error = %format!("{e:#}"), "subagent spawn failed");
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawnFailed {
                            agent_id,
                            error: format!("{e:#}"),
                        },
                    );
                }
            }
        }
        Effect::SpawnSession { binary } => {
            let Some(h) = crate::harness::by_interactive_binary(&binary) else {
                state.notice = Some(format!("'{binary}' is not a session harness"));
                return;
            };
            rt.session_seq += 1;
            let record_id = format!("session-{}", rt.session_seq);
            let model = rt.spawner.model.clone();
            match launch_orchestrator(
                h,
                &OrchestratorCtx {
                    base_url: &rt.spawner.gateway_base,
                    base_repo: &rt.spawner.base_repo,
                    model: model.as_deref(),
                    models: &rt.spawner.models,
                    fleet_sock: rt.spawner.fleet_sock.as_deref(),
                },
                rt.spawner.tx.clone(),
                &record_id,
            ) {
                Ok((rid, _, pty_pane)) => {
                    rt.ptys.insert(rid.clone(), pty_pane);
                    let _ = reduce(
                        state,
                        &AppEvent::SessionSpawned {
                            record_id: rid,
                            binary,
                            model,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(session = %binary, error = %format!("{e:#}"), "session launch failed");
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawnFailed {
                            agent_id: binary,
                            error: format!("{e:#}"),
                        },
                    );
                }
            }
        }
        Effect::CloseAgent { record_id } => {
            if let Some(mut pane) = rt.ptys.remove(&record_id) {
                // A PTY pane close kills the interactive child only — an
                // attach's underlying ACP session is untouched (detach).
                pane.kill();
                return;
            }
            rt.pending.remove(&record_id);
            rt.fleet.leases.remove(&record_id); // drop releases the port
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
        Effect::PtyKey { record_id, key } => {
            if let Some(pane) = rt.ptys.get_mut(&record_id)
                && let Some(bytes) = pane.backend.encode_key(&key)
            {
                pane.write_input(&bytes);
            }
        }
        Effect::Attach { record_id } => {
            attach_interactive(&record_id, state, rt);
        }
        Effect::CancelTurn { record_id } => {
            if let Some(sess) = rt.sessions.get(&record_id) {
                let sess = Arc::clone(sess);
                tokio::spawn(async move {
                    if let Err(e) = sess.cancel().await {
                        tracing::warn!(error = %e, "turn cancel failed");
                    }
                });
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
                let text = match crate::fleet::git_stdout(&meta.worktree, &["diff", &meta.base_ref])
                    .await
                {
                    Ok(t) if t.trim().is_empty() => "(no changes vs the spawn base)".to_string(),
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
            let result = crate::fleet::merge_branch(
                &rt.spawner.base_repo,
                &meta.worktree,
                &meta.branch,
                "ask the agent to commit, or use p (apply)",
            )
            .await;
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
            let result =
                crate::fleet::apply_diff(&rt.spawner.base_repo, &meta.worktree, &meta.base_ref)
                    .await;
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
        #[cfg(unix)]
        Effect::BridgeHello {
            conn,
            bootstrap_approved,
        } => {
            if let Some(writer) = rt.bridges.get_mut(&conn) {
                bridge_write(writer, &crate::fleet::TuiMsg::Hello { bootstrap_approved }).await;
            }
        }
        #[cfg(unix)]
        Effect::BridgeBootstrapApproved => {
            for writer in rt.bridges.values_mut() {
                bridge_write(writer, &crate::fleet::TuiMsg::BootstrapApproved).await;
            }
        }
    }
}

/// Diff-stat the worktree and run the verification checks; `None` when the
/// diff is empty (nothing to review).
async fn inspect_worktree(record_id: &str, meta: &WorkMeta, checks: &[String]) -> Option<Incoming> {
    let stat = crate::fleet::diff_stat(&meta.worktree, &meta.base_ref).await?;
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
                // Step forward to a char boundary: check output can carry
                // multibyte text, and slicing mid-character panics.
                if text.len() > 4000 {
                    let mut start = text.len() - 4000;
                    while !text.is_char_boundary(start) {
                        start += 1;
                    }
                    text = format!("…{}", &text[start..]);
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
    for pane in rt.ptys.values_mut() {
        // PTY children die with the TUI (§2's named asymmetry) — the
        // harness's own --resume/session files are their continuity story,
        // not bitrouter's.
        pane.kill();
    }
    abort_prompt_tasks(&mut rt.prompt_tasks).await;
    // Dropping a PendingPermission defaults it to Deny in the substrate.
    rt.pending.clear();
    // Dropping the leases frees the ports for the next fleet.
    rt.fleet.leases.clear();
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
/// Build the reducer's display-only permission event for one pending request
/// (the resolvable handle stays loop-side in `rt.pending`).
fn perm_event(
    record_id: &str,
    p: &bitrouter_substrate::up::PendingPermission,
    base_repo: &std::path::Path,
) -> AppEvent {
    AppEvent::Permission {
        record_id: record_id.to_string(),
        title: p.tool_call.fields.title.clone().unwrap_or_default(),
        diff: perm_diff(p),
        options: perm_options(p),
        risk: crate::risk::classify(&p.tool_call.fields, base_repo),
    }
}

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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn previous_fleet_notice_summarizes_and_flags_unclean_stops() {
        use bitrouter_substrate::fleet::{FLEET_STATE_VERSION, FleetAgent, FleetState};
        let agent = |review: Option<(u64, u64, u64)>, draft: Option<&str>| FleetAgent {
            record_id: "r".into(),
            autonomy: "manual".into(),
            review,
            port: None,
            pending: None,
            draft: draft.map(str::to_string),
            turn_active: false,
            exited: false,
        };
        let mut prev = FleetState {
            version: FLEET_STATE_VERSION,
            saved_at: 1,
            clean_shutdown: false,
            writer_pid: 1,
            sessions: Vec::new(),
            agents: vec![agent(Some((1, 2, 3)), None), agent(None, Some("d"))],
        };
        let notice = super::previous_fleet_notice(&prev).expect("notice");
        assert!(notice.contains("2 agent(s)"), "{notice}");
        assert!(notice.contains("1 mid-review"), "{notice}");
        assert!(notice.contains("1 unsent draft(s)"), "{notice}");
        assert!(notice.contains("unclean shutdown"), "{notice}");
        assert!(notice.contains("fleet-state.json"), "{notice}");

        prev.clean_shutdown = true;
        let clean = super::previous_fleet_notice(&prev).expect("notice");
        assert!(!clean.contains("unclean"), "{clean}");

        prev.agents.clear();
        assert!(
            super::previous_fleet_notice(&prev).is_none(),
            "an empty fleet is not worth a notice"
        );
    }

    #[test]
    fn harness_tag_names_the_catalog_harness_through_a_runner() {
        let stdio = |command: &str, args: &[&str]| bitrouter_sdk::acp::AcpTransport::Stdio {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: std::collections::HashMap::new(),
        };
        // Runner invocations map to the catalog harness, not the runner.
        assert_eq!(
            super::harness_tag(&stdio(
                "npx",
                &["-y", "@zed-industries/claude-code-acp@latest"]
            )),
            "claude"
        );
        assert_eq!(super::harness_tag(&stdio("codex-acp", &[])), "codex");
        // A non-catalog agent keeps the command basename.
        assert_eq!(
            super::harness_tag(&stdio("/usr/local/bin/my-agent", &[])),
            "my-agent"
        );
    }

    #[test]
    fn fleet_mcp_server_spec_names_the_fleet_backend() {
        let mcp = super::fleet_mcp_server();
        assert_eq!(mcp.name, "bitrouter_fleet");
        assert_eq!(mcp.args, vec!["mcp", "serve", "--backend", "fleet"]);
        assert!(!mcp.command.is_empty());
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
