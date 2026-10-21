//! Single-operation native CLI over the daemon's existing ThreadService.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use bitrouter_orchestrator::thread::ThreadRecoveryRequest;
use clap::{Args, Subcommand, ValueEnum};

use crate::agent_local::{self, Operation, ReplyResult, ThreadClient};
use crate::control_output::{self, Endpoint, PromptFile, Request};
use crate::output::Output;

#[derive(Debug, Clone, Args)]
pub struct Target {
    /// Native BRO Thread ID, never a supervised ACP run ID.
    pub thread_id: String,
    #[command(flatten)]
    pub endpoint: Endpoint,
}

#[derive(Debug, Clone, Args)]
pub struct Submit {
    #[command(flatten)]
    pub target: Target,
    #[command(flatten)]
    pub prompt: PromptFile,
    #[command(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Args)]
pub struct Mutation {
    #[command(flatten)]
    pub target: Target,
    #[command(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read a bounded cold directory page; creates no worker or subscription.
    List {
        #[command(flatten)]
        endpoint: Endpoint,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        cutoff: Option<u64>,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(1..=16))]
        limit: u16,
    },
    /// Create an Ask (or read-only) Thread. Starts no Turn.
    Create {
        #[command(flatten)]
        endpoint: Endpoint,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long)]
        effort: Option<bitrouter_ai::types::ReasoningEffort>,
        #[arg(long)]
        model_policy: bool,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        max_output_tokens: Option<u32>,
        #[arg(long)]
        read_only: bool,
        #[arg(long, conflicts_with = "read_only")]
        check: Option<String>,
        #[command(flatten)]
        request: Request,
    },
    /// Inspect a native Thread or exact Turn, including recovery blockers.
    Show {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        turn: Option<String>,
    },
    /// Read a bounded durable history page without loading execution.
    History {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        cutoff: Option<u64>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
    },
    /// Observe the initial snapshot/catchup; --follow appends NDJSON until Ctrl-C.
    Events {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        after: Option<u64>,
        #[arg(long)]
        follow: bool,
    },
    /// Start one Turn on an existing idle native Thread.
    Start(Submit),
    /// Enqueue one FIFO Turn; never substitutes another task or input ID.
    Enqueue(Submit),
    /// Steer exactly the named active Turn.
    Steer {
        #[command(flatten)]
        submit: Submit,
        #[arg(long)]
        turn: String,
    },
    /// Read the current pending approval without answering it.
    Pending(Target),
    /// Answer exactly one native request on the named Turn.
    Answer {
        #[command(flatten)]
        mutation: Mutation,
        #[arg(long)]
        turn: String,
        #[arg(long)]
        request: String,
        #[arg(value_enum)]
        decision: Decision,
    },
    /// Cancel exactly the active Turn, or --queued to withdraw one queued Turn.
    Cancel {
        #[command(flatten)]
        mutation: Mutation,
        #[arg(long)]
        turn: String,
        #[arg(long)]
        queued: bool,
    },
    /// Explicitly resume the paused FIFO queue; never execution recovery.
    Resume(Mutation),
    /// Withdraw all queued work, cancel/join the current worker, and close.
    Close(Mutation),
    /// Unload safe resident state without deleting durable history.
    Unload(Target),
    /// Reconstruct an OS-local workspace grant and load without continuation.
    Load {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        workspace: PathBuf,
    },
    /// Accept exactly an inspected recovery checkpoint from typed JSON.
    Recover {
        #[command(flatten)]
        target: Target,
        /// ThreadRecoveryRequest: source_server_instance_id, source_cursor, idempotency_key.
        #[arg(long = "from")]
        file: PathBuf,
    },
}

impl Command {
    fn endpoint(&self) -> &Endpoint {
        match self {
            Self::List { endpoint, .. } | Self::Create { endpoint, .. } => endpoint,
            Self::Show { target, .. }
            | Self::History { target, .. }
            | Self::Events { target, .. }
            | Self::Pending(target)
            | Self::Unload(target)
            | Self::Load { target, .. }
            | Self::Recover { target, .. } => &target.endpoint,
            Self::Start(submit) | Self::Enqueue(submit) | Self::Steer { submit, .. } => {
                &submit.target.endpoint
            }
            Self::Answer { mutation, .. }
            | Self::Cancel { mutation, .. }
            | Self::Resume(mutation)
            | Self::Close(mutation) => &mutation.target.endpoint,
        }
    }
}

pub async fn run(command: Command, output: &Output) -> Result<()> {
    let control_socket = command.endpoint().socket().await?;
    let client = ThreadClient::connect(&agent_local::socket_path(&control_socket)).await?;
    command
        .endpoint()
        .check_instance(&client.server_instance_id)?;
    if let Command::Events {
        target,
        after,
        follow,
    } = command
    {
        let mut stream = client.observe(&target.thread_id, after).await?;
        loop {
            let observation = tokio::select! {
                signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); },
                observation = stream.next() => observation?,
            };
            let Some(observation) = observation else {
                return Ok(());
            };
            if follow {
                control_output::event("native_event", &client.server_instance_id, &observation)?;
            } else {
                control_output::emit(
                    output,
                    "native_events",
                    &client.server_instance_id,
                    None,
                    &observation,
                )?;
                return Ok(());
            }
        }
    }
    let mut request_key = None;
    let operation = match command {
        Command::List {
            after,
            cutoff,
            limit,
            ..
        } => Operation::ListThreads {
            after,
            cutoff,
            limit: usize::from(limit),
        },
        Command::Create {
            workspace,
            model,
            effort,
            model_policy,
            max_output_tokens,
            read_only,
            check,
            request,
            ..
        } => {
            let key = request.key()?;
            request_key = Some(key.clone());
            Operation::CreateThread {
                workspace: workspace.canonicalize()?,
                model,
                model_mode: if model_policy {
                    bitrouter_orchestrator::core::protocol::ModelMode::Policy
                } else {
                    bitrouter_orchestrator::core::protocol::ModelMode::Fixed
                },
                effort,
                max_output_tokens,
                read_only,
                skills: None,
                verification_command: check,
                idempotency_key: key,
            }
        }
        Command::Show { target, turn } => match turn {
            Some(turn_id) => Operation::ReadTurn {
                thread_id: target.thread_id,
                turn_id,
            },
            None => Operation::ReadThread {
                thread_id: target.thread_id,
            },
        },
        Command::History {
            target,
            after,
            cutoff,
            limit,
        } => Operation::History {
            thread_id: target.thread_id,
            after,
            cutoff,
            limit: usize::from(limit),
        },
        Command::Pending(target) => {
            let ReplyResult::View { view } = client
                .request(Operation::ReadThread {
                    thread_id: target.thread_id,
                })
                .await?
            else {
                anyhow::bail!("unexpected native Thread view");
            };
            let pending = view.latest_turn.as_ref().and_then(|turn| turn.pending_input.as_ref().map(|input| serde_json::json!({"thread_id":turn.thread_id,"turn_id":turn.turn_id,"request":input})));
            return control_output::emit(
                output,
                "native_pending",
                &client.server_instance_id,
                None,
                &serde_json::json!({"thread_id":view.thread.thread_id,"active_turn_id":view.thread.active_turn_id,"pending":pending}),
            );
        }
        Command::Start(submit) => {
            let key = submit.request.key()?;
            request_key = Some(key.clone());
            Operation::StartTurn {
                thread_id: submit.target.thread_id,
                prompt: submit.prompt.read().await?,
                idempotency_key: key,
            }
        }
        Command::Enqueue(submit) => {
            let key = submit.request.key()?;
            request_key = Some(key.clone());
            Operation::EnqueueTurn {
                thread_id: submit.target.thread_id,
                prompt: submit.prompt.read().await?,
                idempotency_key: key,
            }
        }
        Command::Steer { submit, turn } => {
            let key = submit.request.key()?;
            request_key = Some(key.clone());
            Operation::Steer {
                thread_id: submit.target.thread_id,
                expected_turn_id: turn,
                text: submit.prompt.read().await?,
                idempotency_key: key,
            }
        }
        Command::Answer {
            mutation,
            turn,
            request,
            decision,
        } => {
            let key = mutation.request.key()?;
            request_key = Some(key.clone());
            Operation::Input {
                thread_id: mutation.target.thread_id,
                turn_id: turn,
                request_id: request,
                approved: matches!(decision, Decision::Allow),
                idempotency_key: key,
            }
        }
        Command::Cancel {
            mutation,
            turn,
            queued,
        } => {
            let key = mutation.request.key()?;
            request_key = Some(key.clone());
            if queued {
                Operation::CancelQueuedTurn {
                    thread_id: mutation.target.thread_id,
                    turn_id: turn,
                    idempotency_key: key,
                }
            } else {
                Operation::CancelTurn {
                    thread_id: mutation.target.thread_id,
                    turn_id: turn,
                    idempotency_key: key,
                }
            }
        }
        Command::Resume(mutation) => {
            let key = mutation.request.key()?;
            request_key = Some(key.clone());
            Operation::ResumeQueue {
                thread_id: mutation.target.thread_id,
                idempotency_key: key,
            }
        }
        Command::Close(mutation) => {
            let key = mutation.request.key()?;
            request_key = Some(key.clone());
            Operation::CloseThread {
                thread_id: mutation.target.thread_id,
                idempotency_key: key,
            }
        }
        Command::Unload(target) => Operation::UnloadThread {
            thread_id: target.thread_id,
        },
        Command::Load { target, workspace } => Operation::LoadThread {
            thread_id: target.thread_id,
            workspace: workspace.canonicalize()?,
        },
        Command::Recover { target, file } => {
            let request: ThreadRecoveryRequest = serde_json::from_str(
                &tokio::fs::read_to_string(&file)
                    .await
                    .with_context(|| format!("reading {}", file.display()))?,
            )?;
            control_output::validate_id(&request.idempotency_key)?;
            request_key = Some(request.idempotency_key.clone());
            Operation::RecoverThread {
                thread_id: target.thread_id,
                request,
            }
        }
        Command::Events { .. } => anyhow::bail!("event stream was not dispatched"),
    };
    let reply = match client.request(operation.clone()).await {
        Err(error) if request_key.is_some() && agent_local::uncertain(&error) => client.request(operation).await
            .with_context(|| format!("native operation outcome unconfirmed; retain --request-id {} and --server-instance {}; original error: {error}", request_key.as_deref().unwrap_or("unknown"), client.server_instance_id))?,
        result => result.with_context(|| format!("native operation failed; retain request_id={:?} server_instance={}", request_key, client.server_instance_id))?,
    };
    ensure!(
        !matches!(
            reply,
            ReplyResult::Error { .. } | ReplyResult::UnsupportedVersion { .. }
        ),
        "native operation was rejected"
    );
    control_output::emit(
        output,
        "native_control",
        &client.server_instance_id,
        request_key.as_deref(),
        &reply,
    )
}
