//! Process-scoped Claude CLI lifecycle evidence behind the maintained adapter.
//! Each native process owns a unique spool; ACP ids are not process identities.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::collector::NativeRoot;
use super::types::{Harness, MAX_RECORD_BYTES};

pub const PROXY_NAME: &str = "bitrouter-claude-proxy";

pub const SPOOL_ENV: &str = "BITROUTER_CLAUDE_EVIDENCE_SPOOL";
pub const UPSTREAM_ENV: &str = "BITROUTER_CLAUDE_EVIDENCE_UPSTREAM";
pub const ADAPTER_ENTRY_ENV: &str = "BITROUTER_CLAUDE_ADAPTER_ENTRY";
pub const NAMESPACE_ENV: &str = "BITROUTER_CLAUDE_EVIDENCE_NAMESPACE";

/// The adapter controls pathToClaudeCodeExecutable through this environment
/// override. Only native executables can be substituted without changing how
/// the SDK interprets executable/executableArgs. Script overrides keep their
/// original transport and must not claim process-scoped evidence.
/// <https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts>
/// <https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257>
pub fn prepare_env(
    env: &mut HashMap<String, String>,
    spool: &Path,
    executable: &Path,
    root: &NativeRoot,
) -> Result<bool> {
    // A killed Windows wrapper cannot forward termination to its CLI child.
    // Keep SDK ownership until equivalent process supervision is available.
    if !cfg!(unix) {
        return Ok(false);
    }
    if let Some(original) = env.get("CLAUDE_CODE_EXECUTABLE").cloned() {
        if [".js", ".mjs", ".tsx", ".ts", ".jsx"]
            .iter()
            .any(|suffix| original.ends_with(suffix))
        {
            return Ok(false);
        }
        ensure!(!original.is_empty(), "CLAUDE_CODE_EXECUTABLE is empty");
        env.insert(UPSTREAM_ENV.into(), original);
    }
    let proxy = spool.join(PROXY_NAME);
    // A distinct argv[0] separates adapter CLI probes from ordinary BitRouter
    // commands launched by MCP servers with the adapter's inherited env.
    #[cfg(unix)]
    std::os::unix::fs::symlink(executable, &proxy)?;
    env.insert(
        "CLAUDE_CODE_EXECUTABLE".into(),
        proxy
            .to_str()
            .context("proxy executable must be UTF-8")?
            .into(),
    );
    env.insert(
        SPOOL_ENV.into(),
        spool.to_str().context("native spool must be UTF-8")?.into(),
    );
    env.insert(NAMESPACE_ENV.into(), root.namespace.clone());
    Ok(true)
}

/// Prepare only private subprocess env; ignored malformed Query options stay
/// untouched. The proxy also verifies its actual environment before binding
/// native output to the registered namespace.
pub(super) fn instrument_scope(params: &mut Value, spool: &Path, namespace: &str) -> Result<()> {
    let mut prepared = params.clone();
    let mut cursor = &mut prepared;
    for key in ["_meta", "claudeCode", "options", "env"] {
        let Some(object) = cursor.as_object_mut() else {
            return Ok(());
        };
        cursor = object.entry(key).or_insert_with(|| json!({}));
    }
    let Some(env) = cursor.as_object_mut() else {
        return Ok(());
    };
    env.insert(
        SPOOL_ENV.into(),
        json!(spool.to_str().context("native spool must be UTF-8")?),
    );
    env.insert(NAMESPACE_ENV.into(), json!(namespace));
    *params = prepared;
    Ok(())
}

/// The private executable alias owns all native CLI invocations, including
/// auth probes. Ordinary BitRouter tools can inherit the same environment.
pub fn selected() -> bool {
    std::env::args_os().next().is_some_and(|arg| {
        Path::new(&arg)
            .file_name()
            .is_some_and(|name| name == PROXY_NAME)
    })
}

pub async fn run() -> Result<i32> {
    let program = resolve_upstream(
        std::env::var_os(UPSTREAM_ENV).map(PathBuf::from),
        std::env::var_os("PATH"),
        std::env::var_os(ADAPTER_ENTRY_ENV).map(PathBuf::from),
    )
    .await?;
    ensure!(
        std::fs::canonicalize(&program)? != std::fs::canonicalize(std::env::current_exe()?)?,
        "Claude proxy cannot launch itself"
    );
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let streaming = args.iter().any(|arg| arg == "--input-format")
        && args.iter().any(|arg| arg == "--output-format");
    if !streaming {
        return passthrough(program, args).await;
    }
    let directory =
        PathBuf::from(std::env::var_os(SPOOL_ENV).context("Claude proxy spool missing")?);
    ensure!(directory.is_absolute(), "native spool must be absolute");
    let namespace = std::env::var(NAMESPACE_ENV).context("Claude proxy namespace missing")?;
    let actual = super::service::native_root(Harness::ClaudeCode, &HashMap::new(), &[]);
    let scope_valid = actual
        .as_ref()
        .is_ok_and(|root| root.namespace == namespace);
    run_with(
        program,
        args,
        directory,
        namespace,
        scope_valid,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

/// Auth output never enters the evidence journal. Unix exec retains the
/// adapter's original timeout/signal ownership and native stdio/exit semantics.
/// https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
async fn passthrough(program: PathBuf, args: Vec<OsString>) -> Result<i32> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    restore_environment(&mut command)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        let status = Command::from(command).kill_on_drop(true).status().await?;
        Ok(exit_code(status))
    }
}

fn restore_environment(command: &mut std::process::Command) -> Result<()> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    if std::env::var_os("CLAUDE_CODE_EXECUTABLE")
        .is_some_and(|path| std::fs::canonicalize(path).is_ok_and(|path| path == executable))
    {
        if let Some(original) = std::env::var_os(UPSTREAM_ENV) {
            command.env("CLAUDE_CODE_EXECUTABLE", original);
        } else {
            command.env_remove("CLAUDE_CODE_EXECUTABLE");
        }
    }
    for key in [SPOOL_ENV, UPSTREAM_ENV, ADAPTER_ENTRY_ENV, NAMESPACE_ENV] {
        command.env_remove(key);
    }
    Ok(())
}

async fn resolve_upstream(
    explicit: Option<PathBuf>,
    search: Option<OsString>,
    adapter: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return if path.is_absolute() || path.components().count() > 1 {
            Ok(path)
        } else {
            super::native_runtime::find_executable(&path, search)
        };
    }
    let entry = super::native_runtime::adapter_entry(
        super::native_runtime::CLAUDE,
        adapter,
        search.as_deref(),
    )?;
    let node = super::native_runtime::node(search)?;
    // Match claudeCliPath(): resolve relative to the SDK, including nested
    // optional dependencies and Linux libc preference. No credentials or SDK
    // execution are needed for package resolution.
    // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
    // https://nodejs.org/api/module.html#modulecreaterequirefilename
    let resolver = r#"const {createRequire}=require('node:module');
const sdk=createRequire(process.argv[1]).resolve('@anthropic-ai/claude-agent-sdk');
const req=createRequire(sdk), ext=process.platform==='win32'?'.exe':'';
let names=[`@anthropic-ai/claude-agent-sdk-${process.platform}-${process.arch}/claude${ext}`];
if(process.platform==='linux') { const musl=`@anthropic-ai/claude-agent-sdk-linux-${process.arch}-musl/claude${ext}`; names=process.report?.getReport()?.header?.glibcVersionRuntime?[...names,musl]:[musl,...names]; }
let found; for(const name of names) { try { found=req.resolve(name); break; } catch {} }
if(!found) process.exit(1); process.stdout.write(found);"#;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(node)
            .arg("-e")
            .arg(resolver)
            .arg(entry)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Claude dependency resolution timed out")??;
    ensure!(
        output.status.success() && output.stdout.len() <= 8192,
        "could not resolve the adapter's native Claude; configure CLAUDE_CODE_EXECUTABLE"
    );
    let path = PathBuf::from(String::from_utf8(output.stdout)?);
    ensure!(
        path.is_absolute() && path.is_file(),
        "invalid bundled Claude executable"
    );
    Ok(path)
}

async fn run_with(
    program: PathBuf,
    args: Vec<OsString>,
    directory: PathBuf,
    namespace: String,
    scope_valid: bool,
    input: impl AsyncRead + Unpin + Send,
    output: impl AsyncWrite + Unpin + Send,
) -> Result<i32> {
    let process_id = uuid::Uuid::new_v4().to_string();
    // The controller already creates and registers the private parent. Refuse
    // symlink redirection rather than creating an unregistered spool root.
    let metadata = tokio::fs::symlink_metadata(&directory).await?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "invalid native spool directory"
    );
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(directory.join(format!("cli-{process_id}.jsonl")))
        .await?;
    let tap = Arc::new(Mutex::new(WireTap {
        file,
        process_id: process_id.clone(),
        namespace,
        scope_valid,
        sequence: 0,
        version: None,
    }));
    let mut signals = Signals::new()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    restore_environment(command.as_std_mut())?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tap.lock()
                .await
                .append(json!({"method":"runtime/failed","phase":"metadata"}))
                .await?;
            return Err(error.into());
        }
    };
    tap.lock()
        .await
        .append(json!({"method":"runtime/started","phase":"metadata"}))
        .await?;
    let stdin = child.stdin.take().context("Claude stdin missing")?;
    let stdout = child.stdout.take().context("Claude stdout missing")?;
    let upstream = copy_protocol(input, stdin, "client", Arc::clone(&tap));
    let downstream = copy_protocol(stdout, output, "server", Arc::clone(&tap));
    tokio::pin!(upstream, downstream);
    let mut input_done = false;
    let mut output_done = false;
    let mut exit = None;
    let mut deadline = None;
    let result: Result<()> = async {
        while !(output_done && exit.is_some()) {
            tokio::select! {
                result = &mut upstream, if !input_done => { result?; input_done = true; }
                result = &mut downstream, if !output_done => { result?; output_done = true; }
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status?);
                    input_done = true;
                    deadline = Some(tokio::time::Instant::now() + Duration::from_secs(10));
                }
                signal = signals.next() => {
                    forward_signal(child.id(), signal);
                    // SDK ProcessTransport.close sends SIGKILL after five seconds.
                    // Kill and reap the native child before that wrapper deadline.
                    // https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
                    let at = tokio::time::Instant::now() + Duration::from_secs(3);
                    deadline = Some(deadline.map_or(at, |previous| previous.min(at)));
                }
                () = async { if let Some(at) = deadline { tokio::time::sleep_until(at).await; } else { std::future::pending::<()>().await; } } => {
                    anyhow::bail!("Claude did not exit or drain after termination");
                }
            }
        }
        Ok(())
    }.await;
    if result.is_err() {
        let _ = child.kill().await;
        exit = child.wait().await.ok().or(exit);
    }
    let code = exit.map(exit_code);
    tap.lock().await.append(json!({"method":"runtime/stopped","phase":"metadata","clean":result.is_ok() && code == Some(0),"exit_code":code})).await?;
    result?;
    code.context("Claude process exit status missing")
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

#[cfg(unix)]
struct Signals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}
#[cfg(unix)]
impl Signals {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }
    async fn next(&mut self) -> rustix::process::Signal {
        use rustix::process::Signal;
        tokio::select! { _ = self.term.recv() => Signal::TERM, _ = self.interrupt.recv() => Signal::INT, _ = self.hangup.recv() => Signal::HUP }
    }
}
#[cfg(unix)]
fn forward_signal(pid: Option<u32>, signal: rustix::process::Signal) {
    if let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, signal);
    }
}
#[cfg(not(unix))]
struct Signals;
#[cfg(not(unix))]
impl Signals {
    fn new() -> Result<Self> {
        Ok(Self)
    }
    async fn next(&mut self) {
        std::future::pending::<()>().await;
    }
}
#[cfg(not(unix))]
fn forward_signal(_pid: Option<u32>, _signal: ()) {}

async fn copy_protocol(
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    direction: &str,
    tap: Arc<Mutex<WireTap>>,
) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut overflow = false;
    loop {
        let mut bytes = vec![];
        let count = (&mut input)
            .take(MAX_RECORD_BYTES as u64)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            output.shutdown().await?;
            return Ok(());
        }
        let complete = bytes.last() == Some(&b'\n');
        if !overflow {
            if complete {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(raw) => tap.lock().await.capture(direction, &raw).await?,
                    Err(_) => tap.lock().await.gap("native_cli_invalid_frame").await?,
                }
            } else {
                tap.lock()
                    .await
                    .gap("native_cli_incomplete_or_oversized_frame")
                    .await?;
            }
        }
        overflow = !complete;
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
}

struct WireTap {
    file: tokio::fs::File,
    process_id: String,
    namespace: String,
    scope_valid: bool,
    sequence: u64,
    version: Option<String>,
}
impl WireTap {
    async fn capture(&mut self, direction: &str, raw: &Value) -> Result<()> {
        // CLI NDJSON precedes SDK/ACP transformations. Persist only selected
        // native metadata; user content, control payloads and costs stay out.
        // https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
        if direction == "server" {
            if let Some(selected) = super::claude_sdk::notification_fields(&json!({"message":raw}))
            {
                self.append(json!({"method":"runtime/message","phase":"notification","direction":direction,"payload":selected["message"]})).await?;
            }
        } else if raw.get("type").and_then(Value::as_str) == Some("user") {
            let mut selected = serde_json::Map::new();
            for key in ["type", "uuid", "session_id"] {
                if let Some(value) = raw
                    .get(key)
                    .filter(|value| value.as_str().is_some_and(|value| !value.is_empty()))
                {
                    selected.insert(key.into(), value.clone());
                }
            }
            self.append(json!({"method":"runtime/input","phase":"request","direction":direction,"payload":selected})).await?;
        }
        Ok(())
    }
    async fn gap(&mut self, reason: &str) -> Result<()> {
        self.append(json!({"method":"runtime/gap","phase":"metadata","reason":reason}))
            .await
    }
    async fn append(&mut self, mut event: Value) -> Result<()> {
        if let Some(version) = event
            .pointer("/payload/claude_code_version")
            .and_then(Value::as_str)
        {
            self.version = Some(version.to_owned());
        }
        event["version"] = json!(self.version);
        event["process_id"] = json!(self.process_id);
        event["namespace"] = json!(self.namespace);
        event["scope_valid"] = json!(self.scope_valid);
        event["sequence"] = json!(self.sequence);
        event["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
        let mut bytes = serde_json::to_vec(&event)?;
        ensure!(
            bytes.len() < MAX_RECORD_BYTES,
            "native process evidence size limit"
        );
        bytes.push(b'\n');
        self.file.write_all(&bytes).await?;
        self.file.sync_data().await?;
        self.sequence += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
