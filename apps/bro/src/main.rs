//! Standalone local BRO. Provider configuration and gateway storage belong to the service.
mod acp;
mod catalog;
mod checkpoint;
mod durable;
mod gateway;
mod migrated;
mod orchestration;
mod runner;
#[cfg(test)]
mod runner_tests;
mod skills;
mod tools;
mod tree;
mod update;
mod verification;
mod workspace;

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::types::ReasoningEffort;
use bitrouter_sdk::gateway::GatewayClient;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use checkpoint::{Home, Thread};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4356";

#[derive(Parser)]
#[command(
    name = "bro",
    bin_name = "bro",
    version,
    about = "Local single-user BRO harness for a BitRouter gateway"
)]
struct Cli {
    /// BitRouter gateway URL (BITROUTER_ENDPOINT, default http://127.0.0.1:4356).
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// BRO checkpoint directory (BRO_HOME, default ~/.bro).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Emit machine-readable output. Headless task events always use NDJSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the compiled product identity.
    BuildInfo,
    /// Update the standalone BRO installation.
    Update(update::Options),
    /// Submit a headless coding task.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Inspect and operate a local Thread.
    #[command(visible_alias = "threads")]
    Thread {
        #[command(subcommand)]
        action: ThreadCommand,
    },
    /// Inspect and maintain local Agent Skills directories.
    Skills {
        #[command(subcommand)]
        action: skills::Action,
    },
    /// Schedule child threads in this local BRO home.
    Agent {
        #[command(subcommand)]
        action: orchestration::AgentCommand,
    },
    /// Inspect M0 evidence or bind an imported thread to its local workspace and gateway.
    Managed {
        #[command(subcommand)]
        action: migrated::ManagedCommand,
    },
    /// List or run external ACP agents.
    Acp {
        /// Local agent workspace. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[command(subcommand)]
        command: acp::AcpCommand,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Create a Thread and run until terminal status or an exact tool approval.
    Run {
        prompt: String,
        /// Run this bounded project verification in the workspace sandbox after model completion.
        #[arg(long, conflicts_with = "read_only", value_name = "COMMAND")]
        check: Option<String>,
        #[command(flatten)]
        options: ThreadOptions,
    },
}

#[derive(Args)]
struct ThreadOptions {
    /// Workspace granted to local tools. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, default_value = "bitrouter/auto")]
    model: String,
    #[arg(long)]
    effort: Option<ReasoningEffort>,
    #[arg(long,value_parser=clap::value_parser!(u32).range(1..))]
    max_output_tokens: Option<u32>,
    /// Bind a local project skill by name or directory identity. Repeat to select more.
    #[arg(long = "skill", value_name = "NAME")]
    skills: Vec<String>,
    /// Allow the model to delegate bounded tasks to local read-only child agents.
    #[arg(long)]
    managed: bool,
    /// Expose only read, glob and grep.
    #[arg(long)]
    read_only: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Decision {
    Allow,
    Deny,
}

#[derive(Subcommand)]
enum ThreadCommand {
    List,
    Create {
        #[command(flatten)]
        options: ThreadOptions,
    },
    Show {
        thread_id: Uuid,
        #[arg(long)]
        turn: Option<Uuid>,
    },
    History {
        thread_id: Uuid,
    },
    Pending {
        thread_id: Uuid,
    },
    /// Start one turn in an existing, idle thread.
    Start {
        thread_id: Uuid,
        #[arg(
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        prompt: Option<String>,
        /// UTF-8 file; '-' reads standard input.
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Answer one pending approval and continue through the next checkpoint.
    Answer {
        thread_id: Uuid,
        #[arg(long)]
        turn: Uuid,
        #[arg(long)]
        request: String,
        #[arg(value_enum)]
        decision: Decision,
    },
    /// Continue from a known checkpoint or report its blocker.
    Resume {
        thread_id: Uuid,
        #[arg(long)]
        turn: Uuid,
    },
    /// Cancel the named turn and wait for its local worker to release it.
    Cancel {
        thread_id: Uuid,
        #[arg(long)]
        turn: Uuid,
    },
}

pub(crate) fn emit(value: &impl Serialize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    Ok(out.flush()?)
}

fn data_dir(configured: Option<PathBuf>) -> Result<PathBuf> {
    configured
        .or_else(|| std::env::var_os("BRO_HOME").map(PathBuf::from))
        .or_else(|| std::env::home_dir().map(|p| p.join(".bro")))
        .context("no BRO data directory; pass --data-dir")
}

fn workspace(path: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    }
    .canonicalize()?)
}

fn client(endpoint: &str) -> Result<GatewayClient> {
    Ok(GatewayClient::new(endpoint, token()?.as_deref())?)
}

fn token() -> Result<Option<String>> {
    match std::env::var("BITROUTER_API_KEY") {
        Ok(key) => Ok(Some(key)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("BITROUTER_API_KEY must be valid UTF-8"),
    }
}

fn create(options: ThreadOptions, endpoint: String) -> Result<Thread> {
    ensure!(!options.model.trim().is_empty(), "model must not be empty");
    client(&endpoint)?;
    let mode = if options.read_only {
        ToolMode::ReadOnly
    } else {
        ToolMode::Coding
    };
    let root = workspace(options.workspace)?;
    tools::WorkspaceTools::new(&root, mode)?;
    let mut thread = Thread::new(
        root,
        endpoint,
        options.model,
        options.effort,
        options.max_output_tokens,
        mode,
    )?;
    thread.skills = skills::select(&thread.workspace, &options.skills)?;
    if options.managed {
        thread.enable_managed();
    }
    Ok(thread)
}

fn inspect(thread: &Thread, turn: Option<Uuid>, running: bool) -> Result<()> {
    let t = match turn {
        Some(id) => Some(
            thread
                .turns
                .iter()
                .find(|t| t.turn_id == id)
                .context("unknown turn")?,
        ),
        None => thread.turns.last(),
    };
    emit(
        &json!({"thread_id":thread.thread_id,"workspace":thread.workspace,"model":thread.model,"endpoint":thread.endpoint,"tool_mode":thread.tool_mode,"managed":thread.managed(),
        "turn_id":t.map(|t| t.turn_id),"status":t.map(|t| t.status(running)),"pending_input":t.and_then(|t| t.pending()),
        "verification":t.map(|t| t.verification()), "verification_evidence":t.and_then(|t| t.evidence()),
        "outcome":t.and_then(|t| t.outcome.as_ref())}),
    )
}

fn accepted(thread: &Thread) -> Result<()> {
    emit(
        &json!({"type":"accepted","thread_id":thread.thread_id,"turn_id":thread.current()?.turn_id,"host":"local","workspace":thread.workspace,
        "permission_profile":if thread.tool_mode == ToolMode::ReadOnly {"read_only"} else {"ask"}}),
    )
}

async fn dispatch(cli: Cli) -> Result<i32> {
    if matches!(cli.command, Command::BuildInfo) {
        let revision = env!("BRO_BUILD_REVISION");
        emit(
            &json!({"product":"bitrouter-bro","version":env!("CARGO_PKG_VERSION"),"target":env!("BRO_BUILD_TARGET"),
            "features": if cfg!(feature="http2") { &["http2"][..] } else { &[][..] }, "source_revision":(!revision.is_empty()).then_some(revision)}),
        )?;
        return Ok(0);
    }
    if let Command::Update(options) = cli.command {
        update::run(options).await?;
        return Ok(0);
    }
    if let Command::Skills { action } = cli.command {
        skills::run(action, cli.data_dir)?;
        return Ok(0);
    }
    let root = data_dir(cli.data_dir)?;
    let selected_endpoint = cli
        .endpoint
        .or_else(|| std::env::var("BITROUTER_ENDPOINT").ok());
    let endpoint = selected_endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT);
    if migrated::present(&root)?
        && migrated::dispatch(&root, &cli.command, selected_endpoint.as_deref()).await?
    {
        return Ok(0);
    }
    if let Command::Acp {
        command,
        workspace: path,
    } = cli.command
    {
        return acp::run(
            command,
            selected_endpoint.as_deref(),
            token()?.as_deref(),
            &workspace(path)?,
        )
        .await;
    }
    let home = Home::open(&root)?;
    match cli.command {
        Command::Task {
            action:
                TaskAction::Run {
                    prompt,
                    check,
                    options,
                },
        } => {
            let mut t = create(options, endpoint.into())?;
            if let Some(command) = check {
                t.configure_check(command);
            }
            let _worker = home.worker(t.thread_id)?;
            let client = client(endpoint)?;
            t.start(prompt, None)?;
            if let Some(check) = &mut t.current_mut()?.check {
                check.approval.approved = Some(true);
            }
            home.admit(&t)?;
            accepted(&t)?;
            runner::run(&home, &mut t, &client).await?;
        }
        Command::Thread { action } => {
            thread_command(&home, action, selected_endpoint.as_deref()).await?
        }
        Command::Agent { action } => {
            orchestration::run(&home, action, selected_endpoint.as_deref()).await?
        }
        Command::Managed { .. } => bail!("managed inspection requires an M4 staged BRO store"),
        _ => bail!("unsupported command"),
    }
    Ok(0)
}

async fn thread_command(home: &Home, action: ThreadCommand, endpoint: Option<&str>) -> Result<()> {
    match action {
        ThreadCommand::List => emit(&json!({"threads":home.ids()?})),
        ThreadCommand::Create { options } => {
            let t = create(options, endpoint.unwrap_or(DEFAULT_ENDPOINT).into())?;
            let _worker = home.worker(t.thread_id)?;
            home.admit(&t)?;
            inspect(&t, None, false)
        }
        ThreadCommand::Show { thread_id, turn } => {
            let (_guard, running) = home.observe_worker(thread_id)?;
            inspect(&home.load(thread_id)?, turn, running)
        }
        ThreadCommand::History { thread_id } => emit(&home.load(thread_id)?),
        ThreadCommand::Pending { thread_id } => {
            let t = home.load(thread_id)?;
            emit(
                &json!({"thread_id":thread_id,"turn_id":t.current()?.turn_id,"pending_input":t.current()?.pending()}),
            )
        }
        ThreadCommand::Cancel { thread_id, turn } => {
            let t = home.load(thread_id)?;
            ensure!(t.current()?.turn_id == turn, "current turn ID mismatch");
            if t.current()?.outcome.is_some() {
                return runner::report(&t);
            }
            File::create(home.cancel_path(turn))?.sync_all()?;
            let guard = home.lock_file(thread_id)?;
            guard.lock()?;
            let mut t = home.load(thread_id)?;
            ensure!(
                t.current()?.turn_id == turn,
                "turn changed after cancellation request"
            );
            runner::cancel_turn(home, &mut t)?;
            runner::report(&t)
        }
        ThreadCommand::Start {
            thread_id,
            prompt,
            prompt_file,
            request_id,
        } => {
            let prompt = match (prompt, prompt_file) {
                (Some(prompt), None) => prompt,
                (None, Some(path)) => read_prompt(&path)?,
                _ => bail!("one prompt source required"),
            };
            let (_guard, busy) = home.observe_worker(thread_id)?;
            let mut t = home.load(thread_id)?;
            let client = for_thread(&t, endpoint)?;
            if let Some(key) = &request_id
                && let Some(id) = t.accepted_request(key, &prompt)?
            {
                return inspect(&t, Some(id), busy);
            }
            ensure!(!busy, "another process owns this thread");
            t.start(prompt, request_id)?;
            home.admit(&t)?;
            accepted(&t)?;
            runner::run(home, &mut t, &client).await
        }
        ThreadCommand::Resume { thread_id, turn }
        | ThreadCommand::Answer {
            thread_id, turn, ..
        } => {
            let _worker = home.worker(thread_id)?;
            let mut t = home.load(thread_id)?;
            ensure!(t.current()?.turn_id == turn, "current turn ID mismatch");
            let client = for_thread(&t, endpoint)?;
            if let ThreadCommand::Answer {
                request, decision, ..
            } = action
            {
                runner::answer(home, &mut t, &request, matches!(decision, Decision::Allow))?;
            }
            runner::run(home, &mut t, &client).await
        }
    }
}

fn for_thread(thread: &Thread, endpoint: Option<&str>) -> Result<GatewayClient> {
    ensure!(
        !thread.endpoint.is_empty(),
        "bind the imported thread with managed restore first"
    );
    ensure!(
        endpoint.is_none_or(|e| e == thread.endpoint),
        "gateway endpoint differs from the thread binding"
    );
    client(&thread.endpoint)
}

fn read_prompt(path: &Path) -> Result<String> {
    let mut text = String::new();
    let mut input: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(File::open(path)?)
    };
    input
        .by_ref()
        .take(gateway::MAX_CONTEXT_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= gateway::MAX_CONTEXT_BYTES,
        "prompt exceeds context limit"
    );
    Ok(text)
}

#[tokio::main]
async fn main() -> ExitCode {
    match dispatch(Cli::parse()).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("bro: {error:#}");
            ExitCode::FAILURE
        }
    }
}
