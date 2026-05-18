//! `bitrouter agent session {list,show,close}` — manage named sessions.

use std::io::{self, Write};

use bitrouter::runtime::RuntimePaths;

use super::args::SessionAction;
use super::session::{SessionRecord, SessionStore};
use crate::cli::OutputFormat;

pub fn run(paths: &RuntimePaths, action: SessionAction) -> Result<(), Box<dyn std::error::Error>> {
    let store = SessionStore::new(paths.agent_sessions_file.clone());
    match action {
        SessionAction::List { output } => run_list(&store, output),
        SessionAction::Show { name, output } => run_show(&store, &name, output),
        SessionAction::Close { name } => run_close(&store, &name),
    }
}

fn run_list(store: &SessionStore, format: OutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let sessions = store.list()?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    match format {
        OutputFormat::Json => {
            serde_json::to_writer(&mut stdout, &sessions)?;
            writeln!(stdout)?;
        }
        OutputFormat::Text => {
            if sessions.is_empty() {
                writeln!(stdout, "(no sessions)")?;
            } else {
                writeln!(
                    stdout,
                    "{:<24}  {:<16}  {:<40}",
                    "NAME", "AGENT", "ACP SESSION"
                )?;
                for s in sessions {
                    writeln!(
                        stdout,
                        "{:<24}  {:<16}  {:<40}",
                        s.name, s.agent, s.acp_session_id
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn run_show(
    store: &SessionStore,
    name: &str,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = store
        .load(name)?
        .ok_or_else(|| format!("no session named '{name}'"))?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    match format {
        OutputFormat::Json => {
            serde_json::to_writer(&mut stdout, &record)?;
            writeln!(stdout)?;
        }
        OutputFormat::Text => write_text(&mut stdout, &record)?,
    }
    Ok(())
}

fn run_close(store: &SessionStore, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    store.remove(name)?;
    Ok(())
}

fn write_text(w: &mut impl Write, r: &SessionRecord) -> io::Result<()> {
    writeln!(w, "name:            {}", r.name)?;
    writeln!(w, "agent:           {}", r.agent)?;
    writeln!(w, "acp_session_id:  {}", r.acp_session_id)?;
    writeln!(w, "cwd:             {}", r.cwd.display())?;
    writeln!(w, "created_at:      {}", r.created_at)?;
    writeln!(w, "last_used:       {}", r.last_used)?;
    Ok(())
}
