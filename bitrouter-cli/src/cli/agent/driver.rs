//! Shared event-loop for `bitrouter agent run` and `bitrouter agent attach`.

use std::io::{self, Write};
use std::sync::Arc;

use bitrouter::acp::provider::AcpAgentProvider;
use bitrouter_core::agents::event::{
    AgentEvent, PermissionOutcome, PermissionRequest, PermissionResponse,
};
use bitrouter_core::agents::provider::AgentProvider;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use tokio::sync::Notify;

use super::args::PermissionPolicy;
use super::output::write_event;
use crate::cli::OutputFormat;

/// Terminal state of one `drive_session` invocation.
#[derive(Debug)]
pub enum DriveOutcome {
    /// The turn ended cleanly (`TurnDone` or `Disconnected`).
    Done,
    /// The user pressed Ctrl+C. The driver issued a `session/cancel`
    /// via `AgentProvider::disconnect` and drained the receiver.
    Cancelled,
}

/// Per-turn driver inputs.
///
/// `repl_stdin` is `Some` only in interactive (`agent attach`) mode and
/// is required when `policy` is [`PermissionPolicy::InteractiveStderr`].
/// In all other policies the reader is ignored.
pub struct TurnOpts<'a> {
    pub prompt: String,
    pub policy: PermissionPolicy,
    pub format: OutputFormat,
    pub cancel: Arc<Notify>,
    pub repl_stdin: Option<&'a mut BufReader<Stdin>>,
}

/// Submit one prompt and drive the resulting event stream to completion.
pub async fn drive_session<W: Write>(
    provider: &AcpAgentProvider,
    session_id: &str,
    out: &mut W,
    opts: TurnOpts<'_>,
) -> Result<DriveOutcome, Box<dyn std::error::Error>> {
    let TurnOpts {
        prompt,
        policy,
        format,
        cancel,
        mut repl_stdin,
    } = opts;
    let mut rx = provider.submit(session_id, prompt).await?;

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else {
                    return Ok(DriveOutcome::Done);
                };
                match event {
                    AgentEvent::PermissionRequest { id, request } => {
                        // Emit the request (useful in --output json so
                        // scripts see what was approved); resolve it
                        // according to policy.
                        write_event(
                            out,
                            &AgentEvent::PermissionRequest {
                                id,
                                request: request.clone(),
                            },
                            format,
                        )?;
                        let response = resolve_permission(
                            policy,
                            &request,
                            repl_stdin.as_deref_mut(),
                        )
                        .await?;
                        provider.respond_permission(session_id, id, response).await?;
                    }
                    AgentEvent::TurnDone { .. } | AgentEvent::Disconnected => {
                        write_event(out, &event, format)?;
                        return Ok(DriveOutcome::Done);
                    }
                    AgentEvent::Error { ref message } => {
                        write_event(out, &event, format)?;
                        return Err(message.clone().into());
                    }
                    _ => {
                        write_event(out, &event, format)?;
                    }
                }
            }
            _ = cancel.notified() => {
                // Cooperative cancel: tell the provider to disconnect
                // (which sends ACP session/cancel), then drain residual
                // events so the agent thread shuts down cleanly.
                let _ = provider.disconnect(session_id).await;
                while rx.recv().await.is_some() {}
                return Ok(DriveOutcome::Cancelled);
            }
        }
    }
}

async fn resolve_permission(
    policy: PermissionPolicy,
    request: &PermissionRequest,
    repl_stdin: Option<&mut BufReader<Stdin>>,
) -> io::Result<PermissionResponse> {
    match policy {
        PermissionPolicy::Deny => Ok(deny_response()),
        PermissionPolicy::AutoApprove => Ok(auto_approve_response(request)),
        PermissionPolicy::InteractiveStderr => match repl_stdin {
            Some(reader) => interactive_resolve(reader, request).await,
            // No reader available — fall back to Deny rather than hang.
            None => Ok(deny_response()),
        },
    }
}

fn deny_response() -> PermissionResponse {
    PermissionResponse {
        outcome: PermissionOutcome::Denied,
    }
}

fn auto_approve_response(request: &PermissionRequest) -> PermissionResponse {
    match request.options.first() {
        Some(opt) => PermissionResponse {
            outcome: PermissionOutcome::Allowed {
                selected_option: opt.id.clone(),
            },
        },
        // No options offered — treat as Denied; the agent shouldn't
        // emit an empty options list, but we don't panic if it does.
        None => deny_response(),
    }
}

async fn interactive_resolve(
    reader: &mut BufReader<Stdin>,
    request: &PermissionRequest,
) -> io::Result<PermissionResponse> {
    eprintln!();
    eprintln!("[permission] {}", request.title);
    if !request.description.is_empty() {
        eprintln!("  {}", request.description);
    }
    for (i, opt) in request.options.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, opt.title);
    }
    eprint!("  Approve? [y/N]: ");
    io::stderr().flush()?;

    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        // EOF on stdin — treat as Deny.
        return Ok(deny_response());
    }
    let trimmed = line.trim().to_lowercase();
    if matches!(trimmed.as_str(), "y" | "yes") {
        Ok(auto_approve_response(request))
    } else {
        Ok(deny_response())
    }
}
