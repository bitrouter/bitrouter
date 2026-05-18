//! `bitrouter agent` — headless ACP CLI v1.
//!
//! Subcommands:
//!
//! - `run <agent> "<prompt>"` — one-shot invocation, prints streamed output
//!   to stdout and exits.
//! - `attach <agent>` — interactive REPL with no TUI dependency.
//! - `session {list,show,close}` — manage named, resumable sessions backed
//!   by `<home>/sessions/agent-sessions.json`.
//!
//! Agent installs and registry management remain under the existing
//! plural `bitrouter agents` subcommand; this singular `agent` namespace
//! is exclusively for *session* operations.

mod args;
mod attach;
mod cancel;
mod driver;
mod output;
mod run;
mod session;
mod session_cmd;

use std::path::Path;
use std::sync::Arc;

use bitrouter::acp::provider::AcpAgentProvider;
use bitrouter::runtime::RuntimePaths;
use bitrouter_config::BitrouterConfig;
use bitrouter_core::agents::provider::AgentProvider;

pub use args::AgentCommand;
use session::{SessionRecord, SessionStore};

/// Dispatch a parsed `bitrouter agent ...` invocation.
pub async fn dispatch(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    command: AgentCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        AgentCommand::Run(args) => run::run(config, paths, args).await,
        AgentCommand::Attach(args) => attach::run(config, paths, args).await,
        AgentCommand::Session { action } => session_cmd::run(paths, action),
    }
}

/// Resolve `agent_name` against config + built-in defs and construct an
/// `AcpAgentProvider`. Shared by `run` and `attach`.
fn build_provider(
    config: &BitrouterConfig,
    agent_name: &str,
) -> Result<Arc<AcpAgentProvider>, Box<dyn std::error::Error>> {
    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let agent_config = known
        .get(agent_name)
        .ok_or_else(|| format!("unknown agent: {agent_name}"))?
        .clone();

    if !agent_config.enabled {
        return Err(format!("agent '{agent_name}' is disabled in configuration").into());
    }

    Ok(Arc::new(AcpAgentProvider::new(
        agent_name.to_owned(),
        agent_config,
    )))
}

/// Connect or load an ACP session.
///
/// If `session_name` resolves to a stored record, the session is resumed
/// via `AgentProvider::load_session` and the history-replay receiver is
/// drained without rendering (v1 does not re-print prior history).
/// Otherwise a fresh session is started.
async fn establish(
    provider: &AcpAgentProvider,
    cwd: &Path,
    session_name: Option<&str>,
    store: &SessionStore,
) -> Result<(String, Option<SessionRecord>), Box<dyn std::error::Error>> {
    if let Some(name) = session_name
        && let Some(existing) = store.load(name)?
    {
        let (info, mut rx) = provider.load_session(cwd, &existing.acp_session_id).await?;
        while rx.recv().await.is_some() {}
        return Ok((info.session_id, Some(existing)));
    }
    let info = provider.connect(cwd).await?;
    Ok((info.session_id, None))
}
