//! `bitrouter agent attach <agent>` — interactive REPL with an agent
//! (no TUI).

use std::io;

use bitrouter::runtime::RuntimePaths;
use bitrouter_config::BitrouterConfig;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::args::{AttachArgs, PermissionPolicy};
use super::cancel::cancel_token;
use super::driver::{DriveOutcome, TurnOpts, drive_session};
use super::session::SessionStore;

pub async fn run(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    args: AttachArgs,
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
        PermissionPolicy::InteractiveStderr
    };

    let mut stdout = io::stdout().lock();
    let mut stdin = BufReader::new(tokio::io::stdin());

    eprintln!(
        "attached to {} (session {acp_session_id}). Ctrl+D to exit; Ctrl+C to cancel a turn.",
        args.agent,
    );

    let mut persisted = false;

    loop {
        eprint!("> ");
        use std::io::Write;
        let _ = io::stderr().flush();

        let mut line = String::new();
        let n = stdin.read_line(&mut line).await?;
        if n == 0 {
            // EOF — exit.
            break;
        }
        let prompt = line.trim_end().to_owned();
        if prompt.is_empty() {
            // Empty line — re-prompt without ending the session.
            continue;
        }

        // Re-arm cancel for each turn so Ctrl+C cancels the *current*
        // turn cooperatively, and a second press within the same turn
        // escalates to exit(130). A press while the REPL is awaiting a
        // prompt has no cooperative receiver and is treated as exit.
        let cancel = cancel_token();
        let outcome = drive_session(
            &provider,
            &acp_session_id,
            &mut stdout,
            TurnOpts {
                prompt,
                policy,
                format: args.output,
                cancel,
                repl_stdin: Some(&mut stdin),
            },
        )
        .await;

        match outcome {
            Ok(DriveOutcome::Done) => {
                if !persisted && let Some(name) = args.session.as_deref() {
                    let created = existing.as_ref().map(|r| r.created_at);
                    match store.upsert(name, &args.agent, &acp_session_id, &cwd, created) {
                        Ok(()) => persisted = true,
                        Err(e) => {
                            eprintln!("warning: failed to persist session '{name}': {e}");
                        }
                    }
                }
            }
            Ok(DriveOutcome::Cancelled) => {
                eprintln!("\ncancelled");
            }
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        }
    }

    Ok(())
}
