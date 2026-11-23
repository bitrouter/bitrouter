//! BRO-owned, single-turn headless ACP client for gateway-bound external agents.
mod serve;
#[cfg(test)]
mod tests;
mod translate;
mod wire;

use crate::catalog::{self, Endpoint, Harness};
use agent_client_protocol_schema::{ProtocolVersion, v1::*};
use anyhow::{Context, Result, bail, ensure};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use wire::Peer;

#[derive(clap::Subcommand)]
pub(crate) enum AcpCommand {
    /// Show bundled adapters and whether their headless gateway routing is available.
    List,
    /// Serve the local native worker or a named external ACP adapter over stdio.
    Serve(serve::Options),
    /// Send one prompt to a local adapter. Streams NDJSON and denies tool permission requests.
    Prompt {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        model: Option<String>,
        /// Maximum prompt duration in seconds.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
        turn_timeout: u64,
        text: String,
    },
}

pub(crate) async fn run(
    command: AcpCommand,
    endpoint: Option<&str>,
    token: Option<&str>,
    workspace: Option<std::path::PathBuf>,
    root: &Path,
) -> Result<i32> {
    let mut out = std::io::stdout();
    match command {
        AcpCommand::Serve(options) => {
            serve::run(options, endpoint, token, workspace, root).await?;
            Ok(0)
        }
        AcpCommand::List => {
            for h in catalog::CATALOG {
                let route = if h.adapter.is_some() {
                    "gateway-bound"
                } else {
                    "headless routing unavailable"
                };
                writeln!(out, "{}\t{route}\t{}", h.id, h.description)?;
            }
            Ok(0)
        }
        AcpCommand::Prompt {
            agent,
            model,
            turn_timeout,
            text,
        } => {
            let harness = catalog::find(&agent)?;
            let token = token.context("a gateway token is required for an ACP prompt")?;
            let plan = Endpoint::new(harness, endpoint.unwrap_or(crate::DEFAULT_ENDPOINT), token)?;
            let workspace = crate::workspace(workspace).context("opening ACP workspace")?;
            ensure!(workspace.is_dir(), "ACP workspace must be a directory");
            let mut home = tempfile::Builder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                home.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let home = home
                .prefix("bro-acp-")
                .tempdir()
                .context("creating private ACP adapter home")?;
            let command = agent_command(
                adapter_command(harness),
                harness,
                &plan,
                model.as_deref(),
                home.path(),
            );
            execute(
                command,
                &plan,
                token,
                &workspace,
                &text,
                Duration::from_secs(turn_timeout),
                &mut out,
            )
            .await
        }
    }
}

fn adapter_command(harness: &Harness) -> Command {
    // Windows Node installations provide npx.cmd, which CreateProcess does not
    // resolve from the bare name. Only catalog-pinned package arguments reach it.
    #[cfg(windows)]
    if harness.command == "npx"
        && let Some(path) = std::env::var_os("PATH")
    {
        for dir in std::env::split_paths(&path).filter(|dir| dir.is_absolute()) {
            for runner in ["npx.exe", "npx.cmd"] {
                let candidate = dir.join(runner);
                if candidate.is_file() {
                    return Command::new(candidate);
                }
            }
        }
    }
    Command::new(harness.command)
}

fn agent_command(
    mut cmd: Command,
    harness: &Harness,
    plan: &Endpoint<'_>,
    model: Option<&str>,
    home: &Path,
) -> Command {
    cmd.args(harness.args);
    isolated_environment(&mut cmd, home);
    plan.apply(&mut cmd, model);
    cmd
}

fn isolated_environment(cmd: &mut Command, home: &Path) {
    cmd.env_clear();
    // Forward only runtime plumbing. Provider keys, daemon config, shell hooks and agent-login
    // profiles must not cross into the adapter. session/new supplies the actual workspace.
    for key in [
        "PATH",
        "SystemRoot",
        "SystemDrive",
        "WINDIR",
        "PATHEXT",
        "ComSpec",
        "LANG",
        "LC_CTYPE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    for key in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TMPDIR",
        "TEMP",
        "TMP",
    ] {
        cmd.env(key, home);
    }
    cmd.current_dir(home);
}

struct Process {
    child: Box<dyn ChildWrapper>,
    reaped: bool,
}
impl Drop for Process {
    fn drop(&mut self) {
        // Covers cancellation of the calling future, including during initialize.
        if !self.reaped {
            let _ = self.child.start_kill();
        }
    }
}

impl Process {
    fn spawn(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut wrapped = CommandWrap::from(command);
        #[cfg(unix)]
        wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(process_wrap::tokio::JobObject);
        wrapped.wrap(KillOnDrop);
        Ok(Self {
            child: wrapped.spawn().context("starting the local ACP adapter")?,
            reaped: false,
        })
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Even an adapter that has already exited can leave a package-runner descendant.
        #[cfg(unix)]
        let group = self
            .child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .context("missing ACP process group")?;
        let kill = self.child.start_kill();
        timeout(Duration::from_secs(3), self.child.wait())
            .await
            .context("ACP process cleanup timed out")??;
        self.reaped = true;
        if let Err(error) = kill {
            // Darwin can return EPERM when the exited child is the last zombie in its group.
            // Only accept a failed signal after waiting and confirming the whole group is gone.
            #[cfg(unix)]
            if rustix::process::test_kill_process_group(group) != Err(rustix::io::Errno::SRCH) {
                return Err(error).context("stopping ACP process group");
            }
            #[cfg(not(unix))]
            if error.kind() != std::io::ErrorKind::InvalidInput {
                return Err(error).context("stopping ACP process tree");
            }
        }
        Ok(())
    }
}

async fn execute(
    command: Command,
    plan: &Endpoint<'_>,
    token: &str,
    workspace: &Path,
    text: &str,
    deadline: Duration,
    out: &mut dyn Write,
) -> Result<i32> {
    let mut process = Process::spawn(command)?;
    let mut peer = Peer::new(
        process.child.stdin().take().context("missing ACP stdin")?,
        process
            .child
            .stdout()
            .take()
            .context("missing ACP stdout")?,
        out,
        token,
    );
    let result = tokio::select! {
        result = turn(&mut peer, plan, workspace, text, deadline) => result,
        signal = tokio::signal::ctrl_c() => signal.context("installing ACP interrupt handler")
            .and_then(|()| Err(anyhow::anyhow!("ACP prompt interrupted"))),
    };
    if result.is_err() && peer.session.is_some() {
        // Best effort before unconditional group/job termination. Never retry a paid prompt.
        let _ = timeout(Duration::from_millis(250), peer.cancel()).await;
    }
    drop(peer);
    process.shutdown().await?;
    result
}

async fn turn(
    peer: &mut Peer<'_>,
    plan: &Endpoint<'_>,
    workspace: &Path,
    text: &str,
    deadline: Duration,
) -> Result<i32> {
    let session: NewSessionResponse = timeout(Duration::from_secs(60), async {
        let init: InitializeResponse = peer
            .request("initialize", InitializeRequest::new(ProtocolVersion::V1))
            .await?;
        ensure!(
            init.protocol_version == ProtocolVersion::V1,
            "ACP adapter did not negotiate protocol v1"
        );
        if init.agent_capabilities.providers.is_some() {
            let _: SetProviderResponse = peer.request("providers/set", plan.set_provider()).await?;
            let providers = peer
                .request("providers/list", ListProvidersRequest::new())
                .await?;
            ensure!(
                plan.matches(&providers),
                "ACP adapter did not confirm the selected gateway"
            );
        }
        peer.request("session/new", NewSessionRequest::new(workspace))
            .await
    })
    .await
    .context("ACP adapter startup timed out")??;
    peer.session = Some(session.session_id.clone());
    let prompt = PromptRequest::new(
        session.session_id,
        vec![ContentBlock::Text(TextContent::new(text))],
    );
    let result: PromptResponse =
        match timeout(deadline, peer.request("session/prompt", prompt)).await {
            Ok(response) => response?,
            Err(_) => bail!("ACP prompt timed out"),
        };
    peer.emit(serde_json::json!({"type":"result","stop_reason":result.stop_reason}))?;
    Ok(if peer.denied { 5 } else { 0 })
}
