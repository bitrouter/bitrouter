//! Plain CLI reports and target selection shared by the two session families.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::output::human::Human;
use crate::output::{CliReport, Output};

#[derive(Debug, Clone, clap::Args)]
pub struct Endpoint {
    /// Configuration source used to locate an already-running local daemon.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Owner-local daemon control socket (native commands derive .agent.sock).
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Require this command family's exact server instance; never switch boots.
    #[arg(long)]
    pub server_instance: Option<String>,
}

impl Endpoint {
    pub async fn socket(&self) -> Result<PathBuf> {
        crate::connected_agents::resolve_socket(self.config.as_deref(), self.socket.as_deref())
            .await
    }

    pub fn check_instance(&self, actual: &str) -> Result<()> {
        ensure!(
            self.server_instance
                .as_deref()
                .is_none_or(|expected| expected == actual),
            "server instance changed; expected {:?}, observed {actual}",
            self.server_instance
        );
        Ok(())
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct Request {
    /// Stable operation key. Reuse this exact key and input after an unknown ACK.
    #[arg(long)]
    pub request_id: Option<String>,
}

impl Request {
    pub fn key(&self) -> Result<String> {
        let key = self
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        validate_id(&key)?;
        Ok(key)
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
        "operation identities must contain 1–128 bytes without control characters"
    );
    Ok(())
}

#[derive(Debug, Clone, clap::Args)]
pub struct PromptFile {
    /// UTF-8 prompt file; '-' reads stdin once. Never enters a conversation loop.
    #[arg(long)]
    pub prompt_file: PathBuf,
}

impl PromptFile {
    pub async fn read(&self) -> Result<String> {
        let text = if self.prompt_file.as_os_str() == "-" {
            use tokio::io::AsyncReadExt;
            let mut text = String::new();
            tokio::io::stdin().read_to_string(&mut text).await?;
            text
        } else {
            tokio::fs::read_to_string(&self.prompt_file)
                .await
                .with_context(|| format!("reading prompt {}", self.prompt_file.display()))?
        };
        ensure!(!text.trim().is_empty(), "prompt must not be empty");
        Ok(text)
    }
}

#[derive(Serialize)]
struct Report<'a, T: Serialize> {
    kind: &'a str,
    server_instance_id: &'a str,
    request_id: Option<&'a str>,
    payload: &'a T,
}

impl<T: Serialize> CliReport for Report<'_, T> {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        // JSON-shaped plain text keeps every typed identity, cursor and fence
        // visible without importing a terminal renderer or clipping evidence.
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        for line in text.lines() {
            human.line(line)?;
        }
        Ok(())
    }
}

pub fn emit<T: Serialize>(
    output: &Output,
    kind: &str,
    instance: &str,
    request_id: Option<&str>,
    payload: &T,
) -> Result<()> {
    output.emit(&Report {
        kind,
        server_instance_id: instance,
        request_id,
        payload,
    })?;
    Ok(())
}

pub fn event<T: Serialize>(kind: &str, instance: &str, payload: &T) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(
        &mut stdout,
        &Report {
            kind,
            server_instance_id: instance,
            request_id: None,
            payload,
        },
    )?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}
