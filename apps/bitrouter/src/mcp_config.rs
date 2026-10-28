//! Source-owned MCP configuration. Saving never contacts an upstream or daemon.

mod document;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::mcp::{McpServerConfig, McpTransport};
use crate::gateway::error::BitrouterError;
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::output::reports::config::ConfigActivation;
use crate::output::{CliReport, Output};

#[derive(Subcommand)]
pub enum Command {
    /// List configured MCP servers, with secret values redacted. Does not connect.
    List {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Show one configured MCP server with secret values redacted.
    Show {
        name: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Save a new MCP server. Does not install, connect or restart services.
    Add {
        name: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        input: AddInput,
    },
    /// Replace one complete entry from a file/stdin, or edit it using $EDITOR.
    Edit {
        name: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Complete single McpServerConfig YAML/JSON; '-' reads stdin.
        #[arg(long, value_name = "FILE|-")]
        from: Option<PathBuf>,
    },
    /// Remove only the named configuration entry. Does not stop its running server.
    Remove {
        name: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Args)]
pub struct AddInput {
    /// Streamable HTTP endpoint. Mutually exclusive with --command and --from.
    #[arg(long, conflicts_with_all = ["command", "from"])]
    url: Option<String>,
    /// Stdio server program; arguments follow '--'.
    #[arg(long, conflicts_with = "from")]
    command: Option<String>,
    /// Complete single McpServerConfig YAML/JSON; '-' reads stdin.
    #[arg(long, value_name = "FILE|-")]
    from: Option<PathBuf>,
    #[arg(last = true, requires = "command")]
    args: Vec<String>,
}

/// No file content or content-derived digest is included in a write receipt.
#[derive(Serialize)]
struct ConfigReport {
    source: PathBuf,
    servers: Vec<ConfiguredServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activation: Option<ConfigActivation>,
    running: bitrouter_ai::control::reload::RunningConfigState,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_required: Option<bool>,
    hint: &'static str,
}

#[derive(Serialize)]
struct ConfiguredServer {
    /// Source map key; older configurations may use a different declared name.
    id: String,
    #[serde(flatten)]
    config: McpServerConfig,
}

impl CliReport for ConfigReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(&format!("Source: {}", self.source.display()))?;
        for server in &self.servers {
            let safe = serde_json::to_string(server).map_err(std::io::Error::other)?;
            h.line(&safe)?;
        }
        if self.activation.is_some() {
            h.line("saved_only; running state unknown; MCP startup bindings require restart.")?;
        }
        h.line(self.hint)
    }
}

fn bad(message: impl std::fmt::Display) -> anyhow::Error {
    BitrouterError::bad_request(message).into()
}

fn redacted(mut config: McpServerConfig) -> McpServerConfig {
    match &mut config.transport {
        McpTransport::Http { url, headers } => {
            *url = match url::Url::parse(url) {
                Ok(mut parsed) => {
                    let _ = parsed.set_username("");
                    let _ = parsed.set_password(None);
                    parsed.set_query(None);
                    parsed.set_fragment(None);
                    parsed.to_string()
                }
                Err(_) => "[configured URL]".into(),
            };
            for value in headers.values_mut() {
                *value = "[redacted]".into();
            }
        }
        McpTransport::Stdio { args, env, .. } => {
            for value in env.values_mut().chain(args.iter_mut()) {
                *value = "[redacted]".into();
            }
        }
    }
    config
}

fn input_file(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(std::path::absolute(path)?).context("reading MCP entry input")
    }
}

async fn addition(name: &str, input: AddInput) -> Result<String> {
    if let Some(from) = input.from {
        return input_file(&from);
    }
    let transport = match (input.url, input.command) {
        (Some(url), None) => McpTransport::Http {
            url,
            headers: Default::default(),
        },
        (None, Some(command)) => McpTransport::Stdio {
            command,
            args: input.args,
            env: Default::default(),
        },
        (None, None) if crate::prompt::terminal() => {
            let selected = crate::prompt::select(
                "MCP transport",
                "Saving does not connect or start the server.",
                vec![
                    crate::prompt::Item::new("http", "Streamable HTTP URL"),
                    crate::prompt::Item::new("stdio", "Local server program"),
                ],
                0,
            )
            .await?;
            if selected == 0 {
                McpTransport::Http {
                    url: crate::prompt::line("HTTP URL").await?,
                    headers: Default::default(),
                }
            } else {
                McpTransport::Stdio {
                    command: crate::prompt::line("Program (use --from for arguments/env)").await?,
                    args: Vec::new(),
                    env: Default::default(),
                }
            }
        }
        _ => {
            return Err(bad(
                "mcp add requires --url, --command, or --from; use a terminal for guided input",
            ));
        }
    };
    serde_saphyr::to_string(&McpServerConfig::with_defaults(name, transport))
        .map_err(|_| bad("could not encode MCP entry"))
}

async fn editor(value: &serde_json::Value) -> Result<String> {
    ensure!(
        crate::prompt::terminal(),
        "mcp edit requires --from FILE|- outside a terminal"
    );
    let selected = std::env::var("EDITOR").map_err(|_| bad("set $EDITOR or pass --from FILE|-"))?;
    let words = shell_words::split(&selected)
        .map_err(|_| bad("$EDITOR must be a program with optional quoted arguments"))?;
    let (program, args) = words.split_first().ok_or_else(|| bad("$EDITOR is empty"))?;
    let mut file = tempfile::Builder::new().suffix(".yaml").tempfile()?;
    file.write_all(
        serde_saphyr::to_string(value)
            .map_err(|_| bad("could not encode MCP entry"))?
            .as_bytes(),
    )?;
    file.as_file().sync_all()?;
    let status = tokio::process::Command::new(program)
        .args(args)
        .arg(file.path())
        // Editor output may contain secrets and must never corrupt a JSON result.
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .kill_on_drop(true)
        .status()
        .await
        .context("running $EDITOR")?;
    ensure!(
        status.success(),
        "$EDITOR did not succeed; configuration was not saved"
    );
    std::fs::read_to_string(file.path()).context("reading edited MCP entry")
}

pub async fn execute(command: Command, output: &Output) -> Result<()> {
    // Keep one signal receiver alive across prompt transitions and editor waits.
    crate::prompt::interruptible(execute_inner(command, output)).await
}

async fn execute_inner(command: Command, output: &Output) -> Result<()> {
    let (config, name, write) = match &command {
        Command::List { config } => (config, None, false),
        Command::Show { config, name } => (config, Some(name.as_str()), false),
        Command::Add { config, name, .. }
        | Command::Edit { config, name, .. }
        | Command::Remove { config, name } => (config, Some(name.as_str()), true),
    };
    let path = crate::onboarding::config_path(config.as_deref())?;
    let snapshot = document::Snapshot::read(path)?;
    let values = snapshot.entries()?;
    if !write {
        if config.is_some() && snapshot.raw.is_none() {
            return Err(bad("the selected configuration file does not exist"));
        }
        let selected = if let Some(name) = name {
            vec![(
                name.to_owned(),
                values
                    .get(name)
                    .ok_or_else(|| bad("MCP server is not configured"))?
                    .clone(),
            )]
        } else {
            values.into_iter().collect()
        };
        output.emit(&ConfigReport {
            source: snapshot.path,
            servers: selected
                .into_iter()
                .map(|(id, config)| ConfiguredServer {
                    id,
                    config: redacted(config),
                })
                .collect(),
            activation: None,
            running: bitrouter_ai::control::reload::RunningConfigState::Unknown,
            restart_required: None,
            hint: "Configured entries only. Use bro mcp check to connect and discover tools.",
        })?;
        return Ok(());
    }
    let (name, replacement, add) = match command {
        Command::Add { name, input, .. } => {
            ensure!(
                !values.contains_key(&name),
                "MCP server already exists; use mcp edit"
            );
            let text = addition(&name, input).await?;
            (name, Some(text), true)
        }
        Command::Edit { name, from, .. } => {
            ensure!(values.contains_key(&name), "MCP server is not configured");
            let value = snapshot.entry_value(&name)?;
            let text = match from {
                Some(path) => input_file(&path)?,
                None => crate::prompt::interruptible(editor(&value)).await?,
            };
            (name, Some(text), false)
        }
        Command::Remove { name, .. } => {
            ensure!(values.contains_key(&name), "MCP server is not configured");
            (name, None, false)
        }
        _ => return Err(bad("unsupported MCP mutation")),
    };
    output.emit(&publish(snapshot, &name, replacement.as_deref(), add).await?)?;
    Ok(())
}

/// Wizard and CLI share exactly the same source snapshot and publication path.
pub(crate) async fn add_for_setup(
    config: &Path,
    name: &str,
    from: Option<PathBuf>,
) -> Result<serde_json::Value> {
    let snapshot = document::Snapshot::read(std::path::absolute(config)?)?;
    ensure!(
        !snapshot.entries()?.contains_key(name),
        "MCP server already exists; use mcp edit"
    );
    let text = addition(
        name,
        AddInput {
            url: None,
            command: None,
            from,
            args: Vec::new(),
        },
    )
    .await?;
    Ok(serde_json::to_value(
        publish(snapshot, name, Some(&text), true).await?,
    )?)
}

async fn publish(
    snapshot: document::Snapshot,
    name: &str,
    replacement: Option<&str>,
    add: bool,
) -> Result<ConfigReport> {
    let updated = snapshot.candidate(name, replacement, add)?;
    let validated = document::validate_candidate(&updated)?;
    crate::policy_lock::load_for_config(&validated, Some(&snapshot.path))
        .await
        .map_err(|_| bad("candidate references invalid policy configuration (values omitted)"))?;
    let source = snapshot.path.clone();
    tokio::task::spawn_blocking(move || snapshot.commit(&updated)).await??;
    Ok(ConfigReport {
        source,
        servers: Vec::new(),
        activation: Some(ConfigActivation::SavedOnly),
        running: bitrouter_ai::control::reload::RunningConfigState::Unknown,
        restart_required: Some(true),
        hint: "Saved only. Use bro status for target-owned state; explicitly restart after active work finishes. Route reload does not update native MCP bindings.",
    })
}
