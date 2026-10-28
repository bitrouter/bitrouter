//! Non-interactive ACP inspection/control. No UI attach or lease takeover.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde_json::json;

use super::BackgroundClient;
use crate::client::sessions::wire::{
    ControlFence, LeaseMode, SessionAction, SessionCommand, SessionResponse, SessionScope,
};
use crate::commands::agent_options::SessionSelection;
use crate::control_output::{self, Endpoint, PromptFile, Request};
use crate::daemon::{DaemonCommand, DaemonResponse};
use crate::output::Output;

#[derive(Debug, Clone, Args)]
pub struct Target {
    /// Supervised ACP run ID, never a native BRO Thread ID.
    pub run_id: String,
    #[command(flatten)]
    pub endpoint: Endpoint,
}

#[derive(Debug, Clone, Args)]
pub struct Mutation {
    #[command(flatten)]
    pub target: Target,
    #[command(flatten)]
    pub request: Request,
    /// Reuse the original client only when inspecting/replaying its exact fence.
    #[arg(long)]
    pub client_id: Option<String>,
    /// Exact original generation for an ACK retry; never acquires a new lease.
    #[arg(long, requires_all = ["client_id", "request_id", "server_instance"])]
    pub lease_generation: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum ModeCommand {
    /// Read the last agent-confirmed mode and reported choices.
    Show(Target),
    /// Set one mode on this run's current native session.
    Set {
        #[command(flatten)]
        mutation: Mutation,
        mode: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Read this session's last reported config-option inventory.
    Show(Target),
    /// Set one option using a JSON string, boolean, or ACP typed value object.
    Set {
        #[command(flatten)]
        mutation: Mutation,
        config_id: String,
        #[arg(long)]
        value_json: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RouteCommand {
    /// Read confirmed session routing and available routes; never a global preview.
    Show(Target),
    Set {
        #[command(flatten)]
        mutation: Mutation,
        route: String,
    },
    Reset(Mutation),
}

#[derive(Debug, Subcommand)]
pub enum NativeSessionCommand {
    /// Ask this existing controller for its native session directory.
    List {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cursor: Option<String>,
    },
    New(Mutation),
    Load {
        #[command(flatten)]
        mutation: Mutation,
        session_id: String,
    },
    Resume {
        #[command(flatten)]
        mutation: Mutation,
        session_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read an existing run snapshot without acquiring a control lease.
    Show(Target),
    /// Read retained events; --follow only appends NDJSON and never attaches control.
    Events {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        follow: bool,
    },
    /// Show the exact pending permission IDs and their advertised option IDs.
    Pending(Target),
    /// Submit one prompt to an existing supervised run, then return its ACK.
    Prompt {
        #[command(flatten)]
        mutation: Mutation,
        #[command(flatten)]
        prompt: PromptFile,
    },
    /// Cancel the current ACP turn without stopping the controller.
    Cancel(Mutation),
    /// Answer one existing permission; no approve-all or guessed option IDs.
    Answer {
        #[command(flatten)]
        mutation: Mutation,
        #[arg(long)]
        request: String,
        #[arg(long)]
        option: String,
    },
    /// Mark the current result reviewed without stopping the run.
    Review(Mutation),
    /// Inspect the original boot-local ACK without replaying an operation.
    Action {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        client_id: String,
        #[arg(long, requires = "server_instance")]
        lease_generation: u64,
    },
    /// Inspect or set the current native session's mode.
    Mode {
        #[command(subcommand)]
        action: ModeCommand,
    },
    /// Inspect or set this session's ACP config options.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Inspect/set/reset this run's session route.
    Route {
        #[command(subcommand)]
        action: RouteCommand,
    },
    /// Native session lifecycle on an existing controller; capability-gated.
    Session {
        #[command(subcommand)]
        action: NativeSessionCommand,
    },
}

impl Command {
    fn target(&self) -> &Target {
        match self {
            Self::Show(target)
            | Self::Pending(target)
            | Self::Events { target, .. }
            | Self::Action { target, .. } => target,
            Self::Prompt { mutation, .. }
            | Self::Cancel(mutation)
            | Self::Answer { mutation, .. }
            | Self::Review(mutation) => &mutation.target,
            Self::Mode { action } => match action {
                ModeCommand::Show(target) => target,
                ModeCommand::Set { mutation, .. } => &mutation.target,
            },
            Self::Config { action } => match action {
                ConfigCommand::Show(target) => target,
                ConfigCommand::Set { mutation, .. } => &mutation.target,
            },
            Self::Route { action } => match action {
                RouteCommand::Show(target) => target,
                RouteCommand::Set { mutation, .. } | RouteCommand::Reset(mutation) => {
                    &mutation.target
                }
            },
            Self::Session { action } => match action {
                NativeSessionCommand::List { target, .. } => target,
                NativeSessionCommand::New(mutation)
                | NativeSessionCommand::Load { mutation, .. }
                | NativeSessionCommand::Resume { mutation, .. } => &mutation.target,
            },
        }
    }

    fn identity_and_scopes(&self) -> (Option<String>, Vec<SessionScope>) {
        let mutation = match self {
            Self::Prompt { mutation, .. }
            | Self::Cancel(mutation)
            | Self::Answer { mutation, .. }
            | Self::Review(mutation) => Some(mutation),
            Self::Mode {
                action: ModeCommand::Set { mutation, .. },
            }
            | Self::Config {
                action: ConfigCommand::Set { mutation, .. },
            }
            | Self::Route {
                action: RouteCommand::Set { mutation, .. } | RouteCommand::Reset(mutation),
            }
            | Self::Session {
                action:
                    NativeSessionCommand::New(mutation)
                    | NativeSessionCommand::Load { mutation, .. }
                    | NativeSessionCommand::Resume { mutation, .. },
            } => Some(mutation),
            _ => None,
        };
        if let Some(mutation) = mutation {
            return (
                mutation.client_id.clone(),
                vec![SessionScope::Peek, SessionScope::Respond],
            );
        }
        match self {
            Self::Events { .. } => (None, vec![SessionScope::Transcript]),
            Self::Action { client_id, .. } => (Some(client_id.clone()), vec![SessionScope::Peek]),
            _ => (None, vec![SessionScope::Peek]),
        }
    }
}

fn transport_uncertain(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some()
            || cause
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
            || cause
                .downcast_ref::<serde_json::Error>()
                .is_some_and(serde_json::Error::is_eof)
    })
}

async fn request(client: &BackgroundClient, command: SessionCommand) -> Result<SessionResponse> {
    let send = |command| async {
        tokio::time::timeout(Duration::from_secs(30), client.request(command)).await?
    };
    match send(command.clone()).await {
        Err(error) if transport_uncertain(&error) => send(command).await.with_context(|| {
            format!("same-request ACK retry was unconfirmed; original transport: {error}")
        }),
        result => result,
    }
}

async fn snapshot(
    client: &BackgroundClient,
    run_id: &str,
) -> Result<crate::client::sessions::wire::RunSnapshot> {
    match request(
        client,
        SessionCommand::Snapshot {
            run_id: run_id.into(),
        },
    )
    .await?
    {
        SessionResponse::Snapshot { snapshot } => Ok(*snapshot),
        _ => anyhow::bail!("unexpected supervised snapshot response"),
    }
}

async fn mutate(
    client: &BackgroundClient,
    instance: &str,
    mutation: Mutation,
    action: SessionAction,
    output: &Output,
) -> Result<()> {
    let request_id = mutation.request.key()?;
    let generation = match mutation.lease_generation {
        Some(generation) => generation,
        None => match request(client, SessionCommand::AcquireLease {
            run_id: mutation.target.run_id.clone(), client_id: client.client_id.clone(),
            mode: LeaseMode::Transient, action_request_id: format!("{request_id}:lease"), takeover: false,
        }).await.with_context(|| format!("lease outcome unconfirmed; retain request_id={request_id} client_id={} server_instance={instance}; do not switch identity", client.client_id))? {
            SessionResponse::Lease { lease } => lease.generation,
            _ => anyhow::bail!("unexpected transient lease response"),
        },
    };
    let fence = ControlFence {
        run_id: mutation.target.run_id,
        client_id: client.client_id.clone(),
        lease_generation: generation,
        action_request_id: request_id.clone(),
    };
    let result = request(
        client,
        SessionCommand::Mutate(crate::client::sessions::wire::SessionMutation {
            fence: fence.clone(),
            action,
        }),
    )
    .await;
    // Success already releases transient leases server-side. On rejection or
    // unknown ACK, release only this exact fence; never steal or stop a run.
    if result.is_err() {
        let _ = request(
            client,
            SessionCommand::ReleaseLease {
                fence: ControlFence {
                    action_request_id: format!("{request_id}:release"),
                    ..fence.clone()
                },
            },
        )
        .await;
    }
    match result.with_context(|| format!("ACP operation failed or outcome unknown; inspect original --request-id {request_id} --client-id {} --lease-generation {generation} --server-instance {instance}; do not generate a replacement action", client.client_id))? {
        SessionResponse::Action { acknowledgement } => control_output::emit(output, "acp_action", instance, Some(&request_id), &json!({"client_id":client.client_id,"lease_generation":generation,"acknowledgement":acknowledgement})),
        _ => anyhow::bail!("unexpected supervised mutation response; retain original fence {fence:?}"),
    }
}

pub async fn run(command: Command, output: &Output) -> Result<()> {
    let target = command.target();
    let socket = target.endpoint.socket().await?;
    let instance = match crate::daemon::send_command(&socket, &DaemonCommand::HandoffHello).await? {
        DaemonResponse::HandoffHello { instance_id, .. } => instance_id,
        _ => anyhow::bail!(
            "daemon cannot report its control instance; restart it before session control"
        ),
    };
    target.endpoint.check_instance(&instance)?;
    let (identity, scopes) = command.identity_and_scopes();
    let mut client = BackgroundClient::with_scopes(socket, scopes);
    if let Some(identity) = identity {
        control_output::validate_id(&identity)?;
        client.client_id = identity;
    }
    // A grant is boot-local and never refreshed by this client. Confirm that
    // it was issued by the discovered boot before using it. A later restart
    // invalidates this cached grant rather than authorizing another instance.
    client.grant().await?;
    let DaemonResponse::HandoffHello {
        instance_id: authorized_instance,
        ..
    } = crate::daemon::send_command(client.socket(), &DaemonCommand::HandoffHello).await?
    else {
        anyhow::bail!("daemon cannot confirm the authorized control instance");
    };
    ensure!(
        authorized_instance == instance,
        "server instance changed during authorization; expected {instance}, observed {authorized_instance}; no session operation was submitted"
    );
    let run_id = target.run_id.clone();
    match command {
        Command::Show(_) => control_output::emit(
            output,
            "acp_snapshot",
            &instance,
            None,
            &snapshot(&client, &run_id).await?,
        ),
        Command::Pending(_) => {
            let snapshot = snapshot(&client, &run_id).await?;
            control_output::emit(
                output,
                "acp_pending",
                &instance,
                None,
                &json!({"run_id":snapshot.run_id,"native_session_id":snapshot.native_session_id,"pending":snapshot.pending_permissions}),
            )
        }
        Command::Events { after, follow, .. } => {
            let mut after = after;
            let mut first = true;
            loop {
                let SessionResponse::Events { replay } = request(
                    &client,
                    SessionCommand::Events {
                        run_id: run_id.clone(),
                        after_seq: after,
                    },
                )
                .await?
                else {
                    anyhow::bail!("unexpected supervised event response");
                };
                ensure!(
                    after <= replay.snapshot_seq,
                    "event cursor is ahead of this run; check the run and server instance"
                );
                let next = replay.snapshot_seq;
                let gap = after != 0 && replay.first_retained_seq > after.saturating_add(1);
                let payload =
                    json!({"run_id":run_id,"requested_after":after,"gap":gap,"replay":replay});
                if !follow {
                    return control_output::emit(output, "acp_events", &instance, None, &payload);
                }
                if first || next > after || gap {
                    control_output::event("acp_events", &instance, &payload)?;
                }
                first = false;
                after = next;
                tokio::select! {
                    signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); },
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                }
            }
        }
        Command::Prompt { mutation, prompt } => {
            mutate(
                &client,
                &instance,
                mutation,
                SessionAction::Prompt {
                    text: prompt.read().await?,
                },
                output,
            )
            .await
        }
        Command::Cancel(mutation) => {
            mutate(&client, &instance, mutation, SessionAction::Cancel, output).await
        }
        Command::Answer {
            mutation,
            request,
            option,
        } => {
            mutate(
                &client,
                &instance,
                mutation,
                SessionAction::Permission {
                    permission_id: request,
                    option_id: option,
                },
                output,
            )
            .await
        }
        Command::Review(mutation) => {
            mutate(
                &client,
                &instance,
                mutation,
                SessionAction::MarkReviewed,
                output,
            )
            .await
        }
        Command::Action {
            request_id,
            client_id,
            lease_generation,
            ..
        } => {
            let response = request(
                &client,
                SessionCommand::ActionStatus {
                    fence: ControlFence {
                        run_id,
                        client_id,
                        lease_generation,
                        action_request_id: request_id.clone(),
                    },
                },
            )
            .await?;
            control_output::emit(
                output,
                "acp_action_status",
                &instance,
                Some(&request_id),
                &response,
            )
        }
        Command::Mode { action } => match action {
            ModeCommand::Show(_) => {
                let modes = snapshot(&client, &run_id)
                    .await?
                    .initial_settings
                    .and_then(|settings| settings.modes);
                control_output::emit(
                    output,
                    "acp_mode",
                    &instance,
                    None,
                    &json!({"run_id":run_id,"supported":modes.is_some(),"modes":modes}),
                )
            }
            ModeCommand::Set { mutation, mode } => {
                ensure!(
                    snapshot(&client, &run_id)
                        .await?
                        .initial_settings
                        .is_some_and(|settings| settings.modes.is_some()),
                    "session mode control is unsupported by this agent"
                );
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::SetMode { mode_id: mode },
                    output,
                )
                .await
            }
        },
        Command::Config { action } => match action {
            ConfigCommand::Show(_) => {
                let options = snapshot(&client, &run_id)
                    .await?
                    .initial_settings
                    .and_then(|settings| settings.config_options);
                control_output::emit(
                    output,
                    "acp_config",
                    &instance,
                    None,
                    &json!({"run_id":run_id,"supported":options.is_some(),"options":options}),
                )
            }
            ConfigCommand::Set {
                mutation,
                config_id,
                value_json,
            } => {
                ensure!(
                    snapshot(&client, &run_id)
                        .await?
                        .initial_settings
                        .is_some_and(|settings| settings.config_options.is_some()),
                    "session config control is unsupported by this agent"
                );
                let value = match serde_json::from_str::<serde_json::Value>(&value_json)? {
                    serde_json::Value::String(value) => {
                        agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(value)
                    }
                    serde_json::Value::Bool(value) => value.into(),
                    value => serde_json::from_value(value)
                        .context("expected a JSON string, boolean, or ACP config value object")?,
                };
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::SetConfig { config_id, value },
                    output,
                )
                .await
            }
        },
        Command::Route { action } => match action {
            RouteCommand::Show(_) => control_output::emit(
                output,
                "acp_route",
                &instance,
                None,
                &request(&client, SessionCommand::RouteList { run_id }).await?,
            ),
            RouteCommand::Set { mutation, route } => {
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::RouteSet { route },
                    output,
                )
                .await
            }
            RouteCommand::Reset(mutation) => {
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::RouteClear,
                    output,
                )
                .await
            }
        },
        Command::Session { action } => match action {
            NativeSessionCommand::List { cwd, cursor, .. } => control_output::emit(
                output,
                "acp_native_sessions",
                &instance,
                None,
                &request(
                    &client,
                    SessionCommand::NativeList {
                        run_id,
                        cwd,
                        cursor,
                    },
                )
                .await?,
            ),
            NativeSessionCommand::New(mutation) => {
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::SelectSession {
                        selection: SessionSelection::New,
                    },
                    output,
                )
                .await
            }
            NativeSessionCommand::Load {
                mutation,
                session_id,
            } => {
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::SelectSession {
                        selection: SessionSelection::Load(session_id),
                    },
                    output,
                )
                .await
            }
            NativeSessionCommand::Resume {
                mutation,
                session_id,
            } => {
                mutate(
                    &client,
                    &instance,
                    mutation,
                    SessionAction::SelectSession {
                        selection: SessionSelection::Resume(session_id),
                    },
                    output,
                )
                .await
            }
        },
    }
}
