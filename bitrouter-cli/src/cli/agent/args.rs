//! Clap argument structs for `bitrouter agent` subcommands.
//!
//! These are kept in their own file so the `mod.rs` dispatcher stays focused
//! on routing.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::cli::OutputFormat;

/// `bitrouter agent` — run and manage headless ACP agent sessions.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Run an agent with a single prompt and exit.
    Run(RunArgs),
    /// Start an interactive REPL with an agent (no TUI).
    Attach(AttachArgs),
    /// Manage named, resumable sessions.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Agent name (must be configured and enabled).
    pub agent: String,

    /// Prompt text to send.
    pub prompt: String,

    /// Resume a named session (creates it on first use if absent).
    #[arg(long)]
    pub session: Option<String>,

    /// Working directory advertised to the agent. Defaults to the
    /// current process working directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Auto-approve all permission requests. Without this flag, any
    /// permission request causes the run to abort with a non-zero exit
    /// code.
    #[arg(long)]
    pub yes: bool,

    /// Output format.
    #[arg(long, short = 'o', value_enum, default_value_t = OutputFormat::default())]
    pub output: OutputFormat,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// Agent name (must be configured and enabled).
    pub agent: String,

    /// Resume a named session.
    #[arg(long)]
    pub session: Option<String>,

    /// Working directory advertised to the agent. Defaults to the
    /// current process working directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Auto-approve all permission requests (skip the interactive prompt).
    #[arg(long)]
    pub yes: bool,

    /// Output format.
    #[arg(long, short = 'o', value_enum, default_value_t = OutputFormat::default())]
    pub output: OutputFormat,
}

#[derive(Debug, Subcommand)]
pub enum SessionAction {
    /// List all named sessions.
    List {
        #[arg(long, short = 'o', value_enum, default_value_t = OutputFormat::default())]
        output: OutputFormat,
    },
    /// Show details of a named session.
    Show {
        name: String,
        #[arg(long, short = 'o', value_enum, default_value_t = OutputFormat::default())]
        output: OutputFormat,
    },
    /// Forget a named session (drops the local mapping; does not
    /// disconnect any agent).
    Close { name: String },
}

/// Permission policy for the driver.
///
/// A plain enum — three variants, one match. A trait here would be
/// indirection without payoff until a future `--policy <file.json>`
/// gate is introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Deny every permission request and surface an error.
    Deny,
    /// Approve every permission request automatically.
    AutoApprove,
    /// Prompt the user on stderr for each request.
    InteractiveStderr,
}
