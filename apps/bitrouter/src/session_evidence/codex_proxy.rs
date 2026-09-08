//! A transparent Codex App Server stdio tap used by the maintained ACP adapter.
//! Native sessions remain Codex-owned; this process only journals conversation
//! observations before forwarding their original bytes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::types::{MAX_GRAPH_ITEMS, MAX_RECORD_BYTES};

pub const SPOOL_ENV: &str = "BITROUTER_CODEX_EVIDENCE_SPOOL";
pub const UPSTREAM_ENV: &str = "BITROUTER_CODEX_EVIDENCE_UPSTREAM";
pub const ADAPTER_ENTRY_ENV: &str = "BITROUTER_CODEX_ADAPTER_ENTRY";

/// Runs in the adapter's process environment, where npm has prepended the
/// bundled dependency's bin directory. An explicit CODEX_PATH remains the
/// selected runtime. This hook is implemented by the published ACP adapter:
/// <https://github.com/zed-industries/codex-acp>
pub fn prepare_env(
    env: &mut HashMap<String, String>,
    spool: &Path,
    executable: &Path,
) -> Result<()> {
    if let Some(original) = env.get("CODEX_PATH").cloned() {
        ensure!(!original.is_empty(), "CODEX_PATH is empty");
        env.insert(UPSTREAM_ENV.into(), original);
    }
    env.insert(
        SPOOL_ENV.into(),
        spool
            .to_str()
            .context("native spool path must be UTF-8")?
            .into(),
    );
    env.insert(
        "CODEX_PATH".into(),
        executable
            .to_str()
            .context("controller executable path must be UTF-8")?
            .into(),
    );
    Ok(())
}

/// Private adapter entry point. Stdout contains only upstream JSON-RPC bytes.
pub async fn run() -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os(SPOOL_ENV)
            .context("native App Server proxy requires its controller spool")?,
    );
    ensure!(directory.is_absolute(), "native spool must be absolute");
    let command = resolve_upstream(
        std::env::var_os(UPSTREAM_ENV).map(PathBuf::from),
        std::env::var_os("PATH"),
        std::env::var_os(ADAPTER_ENTRY_ENV).map(PathBuf::from),
    )
    .await?;
    ensure!(
        std::fs::canonicalize(&command.program)?
            != std::fs::canonicalize(std::env::current_exe()?)?,
        "Codex proxy cannot launch itself"
    );
    run_with(command, directory, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn run_with(
    command: RuntimeCommand,
    directory: PathBuf,
    input: impl AsyncRead + Unpin + Send,
    output: impl AsyncWrite + Unpin + Send,
) -> Result<()> {
    tokio::fs::create_dir_all(&directory).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let path = directory.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).await?;
    let tap = Arc::new(Mutex::new(WireTap {
        file,
        requests: BTreeMap::new(),
        sequence: 0,
    }));
    let version = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new(&command.program)
            .args(&command.args)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let version = match version {
        Ok(Ok(output)) if output.status.success() && output.stdout.len() <= 512 => {
            String::from_utf8(output.stdout)
                .ok()
                .map(|version| version.trim().to_owned())
        }
        _ => None,
    };
    tap.lock()
        .await
        .append(json!({"method":"runtime/started","phase":"metadata","version":version}))
        .await?;
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .arg("app-server")
        .env_remove("CODEX_PATH")
        .env_remove(SPOOL_ENV)
        .env_remove(UPSTREAM_ENV)
        .env_remove(ADAPTER_ENTRY_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().context("Codex stdin missing")?;
    let stdout = child.stdout.take().context("Codex stdout missing")?;
    let upstream = copy_protocol(input, stdin, "client", Arc::clone(&tap));
    let downstream = copy_protocol(stdout, output, "server", Arc::clone(&tap));
    let copied = {
        tokio::pin!(upstream, downstream);
        tokio::select! {
            result = &mut downstream => result,
            result = &mut upstream => match result {
                Err(error) => Err(error),
                Ok(()) => tokio::time::timeout(std::time::Duration::from_secs(10), &mut downstream).await
                    .context("Codex did not drain after manager EOF")?,
            },
        }
    };
    if let Err(error) = copied {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
    let success = match status {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
    };
    tap.lock()
        .await
        .append(json!({"method":"runtime/stopped","phase":"metadata","clean":success}))
        .await?;
    ensure!(success, "Codex App Server did not exit cleanly");
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeCommand {
    program: PathBuf,
    args: Vec<PathBuf>,
}

async fn resolve_upstream(
    explicit: Option<PathBuf>,
    search_path: Option<std::ffi::OsString>,
    adapter: Option<PathBuf>,
) -> Result<RuntimeCommand> {
    if let Some(path) = explicit {
        let program = if path.components().count() > 1 || path.is_absolute() {
            path
        } else {
            super::native_runtime::find_executable(&path, search_path)?
        };
        return Ok(RuntimeCommand {
            program,
            args: vec![],
        });
    }
    let entry = adapter_entry(adapter, search_path.as_deref())?;
    let node = super::native_runtime::node(search_path)?;
    // Match the maintained adapter's createRequire(import.meta.url).resolve,
    // including global installs, nested dependency versions and pnpm symlinks.
    // https://nodejs.org/api/module.html#modulecreaterequirefilename
    let resolver = "const {createRequire}=require('node:module'); process.stdout.write(createRequire(process.argv[1]).resolve('@openai/codex/bin/codex.js'));";
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new(&node)
            .arg("-e")
            .arg(resolver)
            .arg(super::native_runtime::module_url(&entry)?.as_str())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Codex dependency resolution timed out")??;
    ensure!(
        output.status.success() && output.stdout.len() <= 8192,
        "could not resolve the adapter's bundled Codex; configure CODEX_PATH explicitly"
    );
    let script = PathBuf::from(String::from_utf8(output.stdout)?);
    ensure!(
        script.is_absolute() && script.is_file(),
        "invalid bundled Codex script path"
    );
    Ok(RuntimeCommand {
        program: node,
        args: vec![script],
    })
}

fn adapter_entry(explicit: Option<PathBuf>, search: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    super::native_runtime::adapter_entry(super::native_runtime::CODEX, explicit, search)
}

async fn copy_protocol(
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    direction: &str,
    tap: Arc<Mutex<WireTap>>,
) -> Result<()> {
    let mut input = BufReader::new(input);
    loop {
        let mut bytes = vec![];
        let count = (&mut input)
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            output.shutdown().await?;
            return Ok(());
        }
        ensure!(
            count <= MAX_RECORD_BYTES && bytes.last() == Some(&b'\n'),
            "incomplete or oversized App Server frame"
        );
        let raw: Value = serde_json::from_slice(&bytes).context("invalid App Server frame")?;
        tap.lock().await.capture(direction, &raw).await?;
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
}

struct WireTap {
    file: File,
    requests: BTreeMap<(String, String), String>,
    sequence: u64,
}

impl WireTap {
    async fn capture(&mut self, direction: &str, raw: &Value) -> Result<()> {
        if let Some(event) = self.select(direction, raw)? {
            self.append(event).await?;
        }
        Ok(())
    }

    fn select(&mut self, direction: &str, raw: &Value) -> Result<Option<Value>> {
        // The public App Server protocol is JSON-RPC over newline-delimited
        // stdio: https://learn.chatgpt.com/docs/app-server
        // Authentication, provider and configuration RPCs are outside this tap.
        if let Some(method) = raw.get("method").and_then(Value::as_str) {
            if !conversation_method(method) {
                return Ok(None);
            }
            if method == "serverRequest/resolved"
                && let Some(id) = raw.pointer("/params/requestId")
            {
                self.requests
                    .remove(&("server".into(), serde_json::to_string(id)?));
            }
            let id = raw.get("id").map(serde_json::to_string).transpose()?;
            if let Some(id) = &id {
                ensure!(
                    self.requests.len() < MAX_GRAPH_ITEMS,
                    "too many pending native requests"
                );
                self.requests
                    .insert((direction.into(), id.clone()), method.into());
            }
            let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
            let payload = if direction == "client" {
                select_fields(
                    &params,
                    &[
                        "threadId",
                        "turnId",
                        "expectedTurnId",
                        "itemId",
                        "lastTurnId",
                        "numTurns",
                        "input",
                        "cwd",
                        "ephemeral",
                        "includeTurns",
                    ],
                )
            } else {
                params
            };
            return Ok(Some(
                json!({"direction":direction,"method":method,"phase":if id.is_some() {"request"} else {"notification"},
                "operation_id":id,"payload":payload}),
            ));
        }
        let Some(id) = raw.get("id") else {
            return Ok(None);
        };
        let id = serde_json::to_string(id)?;
        let origin = if direction == "server" {
            "client"
        } else {
            "server"
        };
        let Some(method) = self.requests.remove(&(origin.into(), id.clone())) else {
            return Ok(None);
        };
        let payload = if let Some(error) = raw.get("error") {
            json!({"error_code":error.get("code")})
        } else if origin == "client" {
            select_fields(
                raw.get("result").unwrap_or(&Value::Null),
                &["thread", "turn", "turnId", "items", "data", "nextCursor"],
            )
        } else {
            raw.get("result").cloned().unwrap_or(Value::Null)
        };
        Ok(Some(
            json!({"direction":direction,"method":method,"phase":"response","operation_id":id,"payload":payload}),
        ))
    }

    async fn append(&mut self, mut event: Value) -> Result<()> {
        event["sequence"] = json!(self.sequence);
        event["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
        let mut bytes = serde_json::to_vec(&event)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "native evidence frame exceeds size limit"
        );
        bytes.push(b'\n');
        self.file.write_all(&bytes).await?;
        self.file.sync_data().await?;
        self.sequence += 1;
        Ok(())
    }
}

fn conversation_method(method: &str) -> bool {
    matches!(
        method,
        "serverRequest/resolved"
            | "error"
            | "thread/start"
            | "thread/resume"
            | "thread/read"
            | "thread/fork"
            | "thread/rollback"
            | "thread/archive"
            | "thread/unarchive"
            | "thread/started"
            | "thread/closed"
            | "thread/status/changed"
            | "thread/tokenUsage/updated"
            | "thread/compacted"
            | "turn/start"
            | "turn/steer"
            | "turn/interrupt"
            | "turn/started"
            | "turn/completed"
            | "turn/diff/updated"
            | "turn/plan/updated"
            | "item/started"
            | "item/completed"
            | "item/agentMessage/delta"
            | "item/plan/delta"
            | "item/commandExecution/outputDelta"
            | "item/commandExecution/terminalInteraction"
            | "item/fileChange/outputDelta"
            | "item/fileChange/patchUpdated"
            | "item/mcpToolCall/progress"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded"
            | "item/reasoning/textDelta"
            | "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/tool/requestUserInput"
    )
}

fn select_fields(params: &Value, names: &[&str]) -> Value {
    let mut result = serde_json::Map::new();
    for name in names {
        if let Some(value) = params.get(*name) {
            result.insert((*name).into(), value.clone());
        }
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests;
