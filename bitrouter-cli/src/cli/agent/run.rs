//! `bitrouter agent run <agent> "<prompt>"` — one-shot headless invocation.

use std::io;

use bitrouter::runtime::RuntimePaths;
use bitrouter_config::BitrouterConfig;

use super::args::{PermissionPolicy, RunArgs};
use super::cancel::cancel_token;
use super::driver::{DriveOutcome, TurnOpts, drive_session};
use super::session::SessionStore;

pub async fn run(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    args: RunArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = super::build_provider(config, &args.agent)?;
    let cwd = match args.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };

    let store = SessionStore::new(paths.agent_sessions_file.clone());
    let (acp_session_id, existing) =
        super::establish(&provider, &cwd, args.session.as_deref(), &store).await?;

    let policy = if args.yes {
        PermissionPolicy::AutoApprove
    } else {
        PermissionPolicy::Deny
    };

    let cancel = cancel_token();
    let mut stdout = io::stdout().lock();

    let outcome = drive_session(
        &provider,
        &acp_session_id,
        &mut stdout,
        TurnOpts {
            prompt: args.prompt,
            policy,
            format: args.output,
            cancel,
            repl_stdin: None,
        },
    )
    .await;

    if let (Some(name), Ok(_)) = (args.session.as_deref(), &outcome) {
        let created = existing.map(|r| r.created_at);
        if let Err(e) = store.upsert(name, &args.agent, &acp_session_id, &cwd, created) {
            eprintln!("warning: failed to persist session '{name}': {e}");
        }
    }

    match outcome? {
        DriveOutcome::Done => Ok(()),
        DriveOutcome::Cancelled => {
            eprintln!("\ncancelled");
            std::process::exit(130);
        }
    }
}
