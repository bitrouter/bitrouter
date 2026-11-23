//! Manager-facing ACP stdio with one supervised adapter per connection.
mod handler;
mod native;

use super::{Endpoint, Process, adapter_command, agent_command, isolated_environment};
use crate::catalog;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(clap::Args)]
pub(crate) struct Options {
    /// Bundled adapter or entry in the explicit local agent configuration.
    #[arg(conflicts_with = "agent")]
    name: Option<String>,
    /// Alternative spelling for the positional adapter name.
    #[arg(long)]
    agent: Option<String>,
    /// Model selector for the local BRO worker.
    #[arg(long, conflicts_with_all = ["name", "agent"])]
    model: Option<String>,
    /// Expose only read-only workspace tools in the native session.
    #[arg(long, conflicts_with_all = ["name", "agent"])]
    read_only: bool,
    /// Launch using the adapter's own configuration and explicit environment.
    #[arg(long)]
    direct: bool,
    /// Local YAML containing `agents.<name>.transport` (stdio command, args and env).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    agents: BTreeMap<String, Agent>,
    #[serde(default)]
    plugins: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Agent {
    transport: Transport,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

pub(super) async fn run(
    options: Options,
    endpoint: Option<&str>,
    token: Option<&str>,
    workspace: Option<PathBuf>,
    root: &Path,
) -> Result<()> {
    let restricted = workspace.is_some();
    let workspace = crate::workspace(workspace)?;
    let Some(name) = options.agent.or(options.name) else {
        ensure!(
            !options.direct && options.config.is_none(),
            "--direct and --config require an external adapter name"
        );
        return native::run(
            root,
            restricted.then_some(workspace.as_path()),
            endpoint.unwrap_or(crate::DEFAULT_ENDPOINT),
            token,
            options.model.as_deref().unwrap_or("bitrouter/auto"),
            options.read_only,
        )
        .await;
    };
    let mut config = match options.config {
        None => Config::default(),
        Some(path) => serde_saphyr::from_str(
            &std::fs::read_to_string(path).context("reading local ACP adapter configuration")?,
        )
        .map_err(|_| anyhow::anyhow!("invalid local ACP adapter configuration"))?,
    };
    for plugin in config.plugins.keys() {
        eprintln!("plugins.{plugin} is not read by this binary and is ignored");
    }
    let home = tempfile::Builder::new().prefix("bro-acp-").tempdir()?;
    let mut plan = None;
    let command = if let Some(agent) = config.agents.remove(&name) {
        ensure!(
            options.direct || (endpoint.is_none() && token.is_none()),
            "using a custom ACP command with gateway settings requires --direct"
        );
        let Transport::Stdio { command, args, env } = agent.transport;
        let mut cmd = Command::new(command);
        isolated_environment(&mut cmd, home.path());
        cmd.current_dir(workspace).args(args).envs(env);
        cmd
    } else {
        let harness = catalog::find(&name)?;
        if options.direct {
            let mut cmd = adapter_command(harness);
            isolated_environment(&mut cmd, home.path());
            cmd.current_dir(workspace).args(harness.args);
            cmd
        } else {
            let bound = Endpoint::new(
                harness,
                endpoint.unwrap_or(crate::DEFAULT_ENDPOINT),
                token.context("gateway token required for a routed ACP adapter")?,
            )?;
            let cmd = agent_command(adapter_command(harness), harness, &bound, None, home.path());
            plan = Some(bound);
            cmd
        }
    };
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut process = Process::spawn(command)?;
    let input = process
        .child
        .stdin()
        .take()
        .context("missing adapter stdin")?;
    let output = process
        .child
        .stdout()
        .take()
        .context("missing adapter stdout")?;
    let terminated = async {
        #[cfg(unix)]
        term.recv().await;
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    let result = tokio::select! {
        result = handler::bridge(input, output, &name, plan.as_ref()) => result,
        result = tokio::signal::ctrl_c() => result.context("waiting for manager interrupt"),
        () = terminated => Ok(()),
    };
    process.shutdown().await?;
    result
}
