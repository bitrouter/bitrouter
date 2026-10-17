//! Real-PTY acceptance infrastructure for the conversation-first Code surface.
//!
//! The reducer and renderer tests can prove state transitions, but they cannot
//! prove raw-mode restoration, bracketed-paste delivery, or the bytes an actual
//! terminal passes to the process. This file keeps those checks deterministic:
//! every child has an isolated directory and mock ACP agent, every observation
//! waits for a semantic condition with a bounded deadline, and no test touches
//! a developer terminal or live credentials.

#![cfg(unix)]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child as ProcessChild, ChildStdin, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

const PTY_TIMEOUT: Duration = Duration::from_secs(15);
const CODE_COLUMNS: u16 = 80;
const CODE_ROWS: u16 = 24;
const REMOTE_FIXTURE_TOKEN: &str = "remote-a12-fixture-token-0123456789";
const ENTER_ALTERNATE_SCREEN: &str = "\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l";
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
// Each fixture has an empty home/cache. Disable remote catalog loading so
// daemon readiness and agent lifecycle tests do not depend on public networking.
const MOCK_DAEMON_CONFIG: &str = "server:\n  listen: 127.0.0.1:0\n  skip_auth: true\nregistry:\n  enabled: false\ndatabase:\n  url: \"sqlite::memory:\"\n";

#[path = "code_tui_pty/evolution.rs"]
mod evolution;

#[path = "code_tui_pty/background.rs"]
mod background;

/// A deliberately small ACP agent. Its responses are protocol-shaped JSON, not
/// terminal snapshots, so test failures describe lifecycle behavior instead of
/// an incidental escape-sequence layout.
#[path = "../../../crates/bitrouter-orchestrator/tests/support/fixture_binary.rs"]
mod fixture_binary;

/// The mode an isolated mock agent should exercise.
#[derive(Debug, Clone, Copy)]
enum MockScenario {
    Minimal,
    DelayedCancel,
    DelayedNormal,
    DelayedRefusal,
    OverlappingPermissions,
    DetachedBacklog,
    SettingsConfirmed,
    SettingsFailure,
    SessionLifecycle,
    FreshSessions,
    HangingSession,
}

impl MockScenario {
    fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::DelayedCancel => "delayed-cancel",
            Self::DelayedNormal => "delayed-normal",
            Self::DelayedRefusal => "delayed-refusal",
            Self::OverlappingPermissions => "overlapping-permissions",
            Self::DetachedBacklog => "detached-backlog",
            Self::SettingsConfirmed => "settings-confirmed",
            Self::SettingsFailure => "settings-failure",
            Self::SessionLifecycle => "session-lifecycle",
            Self::FreshSessions => "fresh-sessions",
            Self::HangingSession => "hanging-session",
        }
    }
}

/// Files and command needed for one mock ACP agent.
struct MockAcp {
    _directory: tempfile::TempDir,
    config_path: PathBuf,
    capture_path: PathBuf,
    control_path: PathBuf,
    home_path: PathBuf,
    bitrouter_home_path: PathBuf,
    temporary_path: PathBuf,
    fixture_binary: PathBuf,
    scenario: MockScenario,
}

impl Drop for MockAcp {
    fn drop(&mut self) {
        // Code now starts a resident owner. Tear down only this fixture's
        // explicit endpoint before deleting its temporary configuration.
        let Ok(text) = std::fs::read_to_string(&self.config_path) else {
            return;
        };
        let Ok(config) = bitrouter_sdk::config::parse_with(&text, |_| None) else {
            return;
        };
        let source = bitrouter::paths::ConfigSource::File(self.config_path.clone());
        let socket = bitrouter::daemon::socket_path_for(&source, &config);
        let Ok(mut stream) = UnixStream::connect(&socket) else {
            return;
        };
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream.write_all(b"{\"cmd\":\"stop\"}\n").is_err() {
            return;
        }
        let mut acknowledgement = String::new();
        let _ = BufReader::new(stream).read_line(&mut acknowledgement);
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl MockAcp {
    async fn shutdown(&self) -> Result<()> {
        let socket = self.supervisor_socket().await?;
        let owner = bitrouter::daemon::probe_status(&socket)
            .await?
            .context("fixture daemon disappeared before cleanup")?;
        ensure!(
            owner.pid != std::process::id(),
            "process cleanup requires a separately owned fixture daemon"
        );
        match bitrouter::daemon::send_command(&socket, &bitrouter::daemon::DaemonCommand::Stop)
            .await?
        {
            bitrouter::daemon::DaemonResponse::Ok => {}
            response => bail!("fixture daemon stop was not acknowledged: {response:?}"),
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while socket.exists() || bitrouter::daemon::process_is_alive(owner.pid) {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "fixture daemon {} did not release its process and endpoint",
                owner.pid
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    fn bro_command(&self) -> Result<tokio::process::Command> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &self.home_path)
            .env("BITROUTER_HOME", &self.bitrouter_home_path)
            .env("TMPDIR", &self.temporary_path)
            .env("NO_COLOR", "1")
            .current_dir(self._directory.path())
            .kill_on_drop(true);
        Ok(command)
    }

    async fn supervisor_socket(&self) -> Result<PathBuf> {
        let source = bitrouter::paths::ConfigSource::File(self.config_path.clone());
        let config = bitrouter::paths::load_config(&source).await?;
        Ok(bitrouter::daemon::socket_path_for(&source, &config))
    }

    async fn supervised_runs(&self) -> Result<Vec<bitrouter::supervisor::RunSnapshot>> {
        let socket = self.supervisor_socket().await?;
        let grant = bitrouter::supervisor::authorize(
            &socket,
            "pty-observer",
            [bitrouter::supervisor::SessionScope::Peek]
                .into_iter()
                .collect(),
        )
        .await?;
        match bitrouter::supervisor::request(
            &socket,
            &grant,
            bitrouter::supervisor::SessionCommand::PeekAll,
        )
        .await?
        {
            bitrouter::supervisor::SessionResponse::Runs { runs } => Ok(runs),
            _ => bail!("supervisor omitted its run inventory"),
        }
    }

    fn new(scenario: MockScenario) -> Result<Self> {
        let directory = tempfile::tempdir().context("creating PTY fixture directory")?;
        let fixture_binary = acp_fixture_executable()?;
        let config_path = directory.path().join("bitrouter.yaml");
        let capture_path = directory.path().join("acp-input.ndjson");
        let control_path = directory.path().join("acp-control.sock");
        let home_path = directory.path().join("home");
        let bitrouter_home_path = directory.path().join("bitrouter-home");
        let temporary_path = directory.path().join("tmp");

        for path in [&home_path, &bitrouter_home_path, &temporary_path] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating isolated directory {}", path.display()))?;
        }

        let command = yaml_string(&fixture_binary)?;
        let capture = yaml_string(&capture_path)?;
        let control = yaml_string(&control_path)?;
        let config = format!(
            "agents:\n\
             \u{20} stub:\n\
             \u{20} \u{20} name: stub\n\
             \u{20} \u{20} transport:\n\
             \u{20} \u{20} \u{20} type: stdio\n\
             \u{20} \u{20} \u{20} command: {command}\n\
             \u{20} \u{20} \u{20} args: []\n\
             \u{20} \u{20} \u{20} env:\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_SCENARIO: {}\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_CAPTURE: {capture}\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_CONTROL: {control}\n",
            scenario.name()
        );
        std::fs::write(&config_path, format!("{MOCK_DAEMON_CONFIG}{config}"))
            .context("writing mock ACP config")?;

        Ok(Self {
            _directory: directory,
            config_path,
            capture_path,
            control_path,
            home_path,
            bitrouter_home_path,
            temporary_path,
            fixture_binary,
            scenario,
        })
    }

    fn start(&self) -> Result<MockProcess> {
        let mut command = Command::new(&self.fixture_binary);
        command
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &self.home_path)
            .env("BITROUTER_HOME", &self.bitrouter_home_path)
            .env("TMPDIR", &self.temporary_path)
            .env("BITROUTER_PTY_SCENARIO", self.scenario.name())
            .env("BITROUTER_PTY_CAPTURE", &self.capture_path)
            .env("BITROUTER_PTY_CONTROL", &self.control_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().with_context(|| {
            format!(
                "starting mock ACP process {}",
                self.fixture_binary.display()
            )
        })?;
        let stdin = child.stdin.take().context("taking mock ACP stdin")?;
        let stdout = child.stdout.take().context("taking mock ACP stdout")?;
        MockProcess::new(child, stdin, stdout)
    }

    fn captured_prompts(&self) -> Result<Vec<String>> {
        let bytes = match std::fs::read(&self.capture_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading captured ACP input"),
        };
        let mut prompts = Vec::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(line).context("parsing captured ACP input")?;
            if value.get("method").and_then(serde_json::Value::as_str) == Some("session/prompt")
                && let Some(text) = find_text(&value)
            {
                prompts.push(text);
            }
        }
        Ok(prompts)
    }

    fn captured_request(&self, method: &str) -> Result<Option<serde_json::Value>> {
        let bytes = match std::fs::read(&self.capture_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading captured ACP input"),
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(line).context("parsing captured ACP input")?;
            if value.get("method").and_then(serde_json::Value::as_str) == Some(method) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn wait_for_request(&self, method: &str) -> Result<serde_json::Value> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            if let Some(request) = self.captured_request(method)? {
                return Ok(request);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for mock ACP request {method}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn captured_prompt(&self) -> Result<Option<String>> {
        Ok(self.captured_prompts()?.into_iter().next())
    }

    fn captured_permission_outcomes(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let bytes = match std::fs::read(&self.capture_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading captured ACP input"),
        };
        let mut outcomes = Vec::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(line).context("parsing captured ACP input")?;
            let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !matches!(id, "permission-1" | "permission-2") {
                continue;
            }
            if let Some(outcome) = value.get("result").and_then(|result| result.get("outcome")) {
                outcomes.push((id.to_string(), outcome.clone()));
            }
        }
        Ok(outcomes)
    }

    fn wait_for_permission_outcomes(
        &self,
        expected: usize,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let outcomes = self.captured_permission_outcomes()?;
            if outcomes.len() >= expected {
                return Ok(outcomes);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for {expected} permission outcomes; received {outcomes:?}"
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn release(&self) -> Result<()> {
        self.control("release")
    }

    fn stop_with_refusal(&self) -> Result<()> {
        self.control("refusal")
    }

    fn disconnect(&self) -> Result<()> {
        self.control("disconnect")
    }

    fn release_backlog(&self) -> Result<()> {
        self.control("backlog")
    }

    fn control(&self, message: &str) -> Result<()> {
        let mut stream = UnixStream::connect(&self.control_path).with_context(|| {
            format!(
                "connecting to mock ACP control {}",
                self.control_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(PTY_TIMEOUT))
            .context("bounding mock ACP control response")?;
        stream
            .set_write_timeout(Some(PTY_TIMEOUT))
            .context("bounding mock ACP control write")?;
        stream
            .write_all(message.as_bytes())
            .context("sending mock ACP control command")?;
        stream
            .write_all(b"\n")
            .context("terminating mock ACP control command")?;
        stream
            .flush()
            .context("flushing mock ACP control command")?;
        let mut acknowledgement = [0_u8; 3];
        stream
            .read_exact(&mut acknowledgement)
            .context("reading mock ACP control acknowledgement")?;
        ensure!(
            acknowledgement == *b"ok\n",
            "unexpected mock ACP control acknowledgement {acknowledgement:?}"
        );
        Ok(())
    }
}

/// The responses one isolated remote-control target exposes to Code.
#[derive(Debug, Clone, Copy)]
enum RemoteScenario {
    Healthy,
    ModelsFailure,
}

/// One HTTP request that reached the isolated remote target.
#[derive(Debug)]
struct RemoteRequest {
    method: String,
    target: String,
    authorization: Option<String>,
    body: String,
}

/// A small loopback remote-control listener and its isolated context store.
///
/// The fixture intentionally implements HTTP directly. It captures the exact
/// target, bearer header, and route-preview body without relying on the local
/// daemon, an inherited credential, or a paid upstream.
struct RemoteFixture {
    _directory: tempfile::TempDir,
    home_path: PathBuf,
    bitrouter_home_path: PathBuf,
    temporary_path: PathBuf,
    address: SocketAddr,
    requests: Receiver<Result<RemoteRequest, String>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RemoteFixture {
    fn bro_command(&self) -> Result<tokio::process::Command> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(["--context", "fixture"])
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &self.home_path)
            .env("BITROUTER_HOME", &self.bitrouter_home_path)
            .env("TMPDIR", &self.temporary_path)
            .env("NO_COLOR", "1")
            .env("BITROUTER_A12_TOKEN", REMOTE_FIXTURE_TOKEN)
            .current_dir(self._directory.path())
            .kill_on_drop(true);
        Ok(command)
    }

    fn new(scenario: RemoteScenario) -> Result<Self> {
        let directory = tempfile::tempdir().context("creating remote PTY fixture directory")?;
        let home_path = directory.path().join("home");
        let bitrouter_home_path = directory.path().join("bitrouter-home");
        let temporary_path = directory.path().join("tmp");
        for path in [&home_path, &bitrouter_home_path, &temporary_path] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating isolated directory {}", path.display()))?;
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding isolated remote-control fixture")?;
        let address = listener
            .local_addr()
            .context("reading isolated remote-control fixture address")?;
        let endpoint = format!("http://{address}");
        let contexts = format!(
            "version = 1\n\n[contexts.fixture]\nendpoint = {endpoint:?}\ntoken_env = \"BITROUTER_A12_TOKEN\"\n"
        );
        std::fs::write(bitrouter_home_path.join("contexts.toml"), contexts)
            .context("writing isolated remote context store")?;

        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let (sender, requests) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("bitrouter-remote-control-fixture".to_string())
            .spawn(move || serve_remote_fixture(listener, scenario, worker_stopped, sender))
            .context("starting isolated remote-control fixture")?;

        Ok(Self {
            _directory: directory,
            home_path,
            bitrouter_home_path,
            temporary_path,
            address,
            requests,
            stopped,
            worker: Some(worker),
        })
    }

    fn wait_for_request(&self, method: &str, target_prefix: &str) -> Result<RemoteRequest> {
        match self.requests.recv_timeout(PTY_TIMEOUT) {
            Ok(Ok(request))
                if request.method == method && request.target.starts_with(target_prefix) =>
            {
                Ok(request)
            }
            Ok(Ok(request)) => {
                bail!(
                    "unexpected remote request {} {}; expected {method} {target_prefix}",
                    request.method,
                    request.target
                );
            }
            Ok(Err(error)) => bail!("remote fixture failed: {error}"),
            Err(RecvTimeoutError::Timeout) => {
                bail!("timed out waiting for remote {method} {target_prefix}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("remote fixture stopped while waiting for {method} {target_prefix}")
            }
        }
    }

    fn assert_bearer(&self, request: &RemoteRequest) -> Result<()> {
        let expected = format!("Bearer {REMOTE_FIXTURE_TOKEN}");
        ensure!(
            request.authorization.as_deref() == Some(expected.as_str()),
            "remote request did not carry the fixture bearer: {request:?}"
        );
        Ok(())
    }
}

impl Drop for RemoteFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_remote_fixture(
    listener: TcpListener,
    scenario: RemoteScenario,
    stopped: Arc<AtomicBool>,
    sender: mpsc::Sender<Result<RemoteRequest, String>>,
) {
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) => {
                if !stopped.load(Ordering::Acquire) {
                    let _ = sender.send(Err(format!("accepting remote fixture request: {error}")));
                }
                return;
            }
        };
        if stopped.load(Ordering::Acquire) {
            return;
        }
        let request = match read_remote_request(&mut stream) {
            Ok(request) => request,
            Err(error) => {
                let _ = sender.send(Err(format!("reading remote fixture request: {error:#}")));
                return;
            }
        };
        let (status, body) = remote_response(scenario, &request);
        if let Err(error) = write_remote_response(&mut stream, status, body) {
            let _ = sender.send(Err(format!("writing remote fixture response: {error:#}")));
            return;
        }
        if sender.send(Ok(request)).is_err() {
            return;
        }
    }
}

fn read_remote_request(stream: &mut TcpStream) -> Result<RemoteRequest> {
    stream
        .set_read_timeout(Some(PTY_TIMEOUT))
        .context("bounding remote fixture request")?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            break position;
        }
        ensure!(
            bytes.len() < 64 * 1024,
            "remote fixture request headers exceeded 64 KiB"
        );
        let count = stream
            .read(&mut buffer)
            .context("reading remote fixture request")?;
        ensure!(
            count > 0,
            "remote fixture client closed before request headers"
        );
        bytes.extend_from_slice(&buffer[..count]);
    };
    let header = std::str::from_utf8(&bytes[..header_end])
        .context("decoding remote fixture request headers")?;
    let mut lines = header.lines();
    let request_line = lines
        .next()
        .context("reading remote fixture request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .context("reading remote fixture request method")?
        .to_string();
    let target = request_parts
        .next()
        .context("reading remote fixture request target")?
        .to_string();
    ensure!(
        request_parts.next().is_some(),
        "remote fixture request did not include an HTTP version"
    );
    let mut authorization = None;
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse::<usize>()
                .context("parsing remote fixture content length")?;
        }
    }
    let body_start = header_end + 4;
    while bytes.len().saturating_sub(body_start) < content_length {
        let count = stream
            .read(&mut buffer)
            .context("reading remote fixture request body")?;
        ensure!(
            count > 0,
            "remote fixture client closed before request body"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body_end = body_start.saturating_add(content_length);
    let body = std::str::from_utf8(&bytes[body_start..body_end])
        .context("decoding remote fixture request body")?
        .to_string();
    Ok(RemoteRequest {
        method,
        target,
        authorization,
        body,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn remote_response(scenario: RemoteScenario, request: &RemoteRequest) -> (u16, &'static str) {
    let target = request.target.split('?').next().unwrap_or_default();
    match (request.method.as_str(), target) {
        ("GET", "/control/v1/capabilities") => (
            200,
            r#"{"protocol":"bitrouter-control","protocol_version":1,"server_version":"REMOTE_A12_CAPABILITIES","actions":["status","models","route_preview","requests"]}"#,
        ),
        ("GET", "/control/v1/status") => (
            200,
            r#"{"running":true,"pid":4242,"listen":"REMOTE_A12_STATUS","models":1,"providers":["REMOTE_A12_PROVIDER"]}"#,
        ),
        ("GET", "/control/v1/models") if matches!(scenario, RemoteScenario::ModelsFailure) => (
            502,
            r#"{"error":{"code":"fixture_unavailable","message":"REMOTE_A12_MODELS_FAILURE"}}"#,
        ),
        ("GET", "/control/v1/models") => (
            200,
            r#"{"models":[{"id":"REMOTE_A12_MODEL","providers":["REMOTE_A12_PROVIDER"]}],"resolved_via":"live"}"#,
        ),
        ("GET", "/control/v1/requests") => (
            200,
            r#"{"mode":"live","daemon":{"pid":4242,"listen":"REMOTE_A12_REQUESTS","models":1},"window":"REMOTE_A12_WINDOW","scope":"REMOTE_A12_SCOPE","spend_micro_usd":0,"requests":1,"unpriced_requests":0,"requests_per_minute":1.0,"tokens_per_minute":2.0,"rows":[],"filters":{"since":"2026-01-01T00:00:00Z","until":"2026-01-02T00:00:00Z","model":null,"provider":null},"truncated":false,"metering":{"summary":"available","rate":"available","rows":"available"},"rate_scope":"all callers, trailing minute"}"#,
        ),
        ("POST", "/control/v1/route/preview") => (
            200,
            r#"{"requested_model":"REMOTE_A12_MODEL","effective_model":"REMOTE_A12_MODEL","resolved_via":"live","provider_chain":[{"provider":"REMOTE_A12_ROUTE_RESULT","service_id":"remote-a12-service","api_protocol":"fixture"}]}"#,
        ),
        _ => (
            404,
            r#"{"error":{"code":"fixture_not_found","message":"REMOTE_A12_UNEXPECTED_REQUEST"}}"#,
        ),
    }
}

fn write_remote_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Fixture Response",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("writing remote fixture HTTP response")?;
    stream
        .flush()
        .context("flushing remote fixture HTTP response")
}

/// A bounded JSON-RPC client for exercising the mock agent itself.
struct MockProcess {
    child: ProcessChild,
    stdin: ChildStdin,
    receiver: Receiver<Result<serde_json::Value, String>>,
    pending: VecDeque<serde_json::Value>,
}

impl MockProcess {
    fn new(
        child: ProcessChild,
        stdin: ChildStdin,
        stdout: std::process::ChildStdout,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("bitrouter-mock-acp-reader".to_string())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let message = match line {
                        Ok(line) => serde_json::from_str(&line).map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    if sender.send(message).is_err() {
                        return;
                    }
                }
            })
            .context("starting mock ACP reader")?;
        Ok(Self {
            child,
            stdin,
            receiver,
            pending: VecDeque::new(),
        })
    }

    fn send(&mut self, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &value).context("encoding mock ACP request")?;
        self.stdin
            .write_all(b"\n")
            .context("terminating mock ACP request")?;
        self.stdin.flush().context("flushing mock ACP request")
    }

    fn request(
        &mut self,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.wait_for(&format!("response {id}"), |value| {
            value.get("id").and_then(serde_json::Value::as_str) == Some(id)
        })
    }

    fn wait_for<F>(&mut self, description: &str, predicate: F) -> Result<serde_json::Value>
    where
        F: Fn(&serde_json::Value) -> bool,
    {
        if let Some(index) = self.pending.iter().position(&predicate)
            && let Some(value) = self.pending.remove(index)
        {
            return Ok(value);
        }

        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for {description}");
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(Ok(value)) if predicate(&value) => return Ok(value),
                Ok(Ok(value)) => self.pending.push_back(value),
                Ok(Err(error)) => {
                    bail!("mock ACP reader failed while waiting for {description}: {error}")
                }
                Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for {description}"),
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("mock ACP exited while waiting for {description}")
                }
            }
        }
    }
}

impl Drop for MockProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// A real pseudo-terminal whose output is accumulated until a semantic
/// condition is observed. It never uses a fixed sleep to make progress.
struct PtyRunner {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Vec<u8>,
    screen: vt100::Parser,
    output_receiver: Receiver<Result<Vec<u8>, String>>,
    exit_receiver: Receiver<Result<portable_pty::ExitStatus, String>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// A point-in-time view of the terminal screen and raw stream.
///
/// The screen has no scrollback, so the snapshot describes only what a user
/// can currently see, rather than text retained from an earlier frame.
#[derive(Debug, Clone)]
struct PtyCheckpoint {
    output_len: usize,
    screen: String,
}

impl PtyRunner {
    fn spawn(command: CommandBuilder, columns: u16, rows: u16) -> Result<Self> {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening test PTY")?;
        let reader = pty
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let writer = pty.master.take_writer().context("taking PTY writer")?;
        let mut child = pty
            .slave
            .spawn_command(command)
            .context("spawning PTY child")?;
        let killer = child.clone_killer();

        let (output_sender, output_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("bitrouter-pty-reader".to_string())
            .spawn(move || {
                let mut reader = reader;
                let mut buffer = [0_u8; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(count) => {
                            if output_sender.send(Ok(buffer[..count].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = output_sender.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
            })
            .context("starting PTY reader")?;

        let (exit_sender, exit_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("bitrouter-pty-waiter".to_string())
            .spawn(move || {
                let result = child.wait().map_err(|error| error.to_string());
                let _ = exit_sender.send(result);
            })
            .context("starting PTY waiter")?;

        Ok(Self {
            master: pty.master,
            writer,
            output: Vec::new(),
            screen: vt100::Parser::new(rows, columns, 0),
            output_receiver,
            exit_receiver,
            killer,
        })
    }

    fn resize(&mut self, columns: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing test PTY")?;
        self.screen.set_size(rows, columns);
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("writing PTY input")?;
        self.writer.flush().context("flushing PTY input")
    }

    fn checkpoint(&self) -> PtyCheckpoint {
        PtyCheckpoint {
            output_len: self.output.len(),
            screen: self.screen.screen().contents(),
        }
    }

    fn wait_for_text(&mut self, text: &str) -> Result<String> {
        self.wait_for_text_inner(None, text)
    }

    fn wait_for_text_since(&mut self, checkpoint: &PtyCheckpoint, text: &str) -> Result<String> {
        ensure!(
            checkpoint.output_len <= self.output.len(),
            "PTY checkpoint was beyond its captured output"
        );
        self.wait_for_text_inner(Some(checkpoint), text)
    }

    fn wait_for_text_inner(
        &mut self,
        checkpoint: Option<&PtyCheckpoint>,
        text: &str,
    ) -> Result<String> {
        self.wait_for_screen_inner(checkpoint, text, |screen| screen.contains(text))
    }

    fn wait_for_screen_inner(
        &mut self,
        checkpoint: Option<&PtyCheckpoint>,
        text: &str,
        matches: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let screen = self.screen.screen().contents();
            let visible = matches(&screen);
            let changed_since_checkpoint = checkpoint.is_none_or(|checkpoint| {
                self.output[checkpoint.output_len..]
                    .windows(text.len())
                    .any(|window| window == text.as_bytes())
                    || screen != checkpoint.screen
            });
            if visible && changed_since_checkpoint && synchronized_update_complete(&self.output) {
                return Ok(screen);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for visible PTY text {text:?}; screen was {screen:?}; raw output was {:?}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            match self.output_receiver.recv_timeout(remaining) {
                Ok(Ok(bytes)) => {
                    self.screen.process(&bytes);
                    self.output.extend_from_slice(&bytes);
                }
                Ok(Err(error)) => {
                    bail!(
                        "PTY reader failed while waiting for {text:?}: {error}; screen was {screen:?}"
                    )
                }
                Err(RecvTimeoutError::Timeout) => {
                    bail!(
                        "timed out waiting for visible PTY text {text:?}; screen was {screen:?}; raw output was {:?}",
                        String::from_utf8_lossy(&self.output)
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("PTY closed while waiting for {text:?}; screen was {screen:?}")
                }
            }
        }
    }

    fn wait_for_raw_text(&mut self, text: &str) -> Result<String> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let transcript = String::from_utf8_lossy(&self.output).to_string();
            if transcript.contains(text) {
                return Ok(transcript);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for raw PTY text {text:?}; output was {transcript:?}");
            }
            match self.output_receiver.recv_timeout(remaining) {
                Ok(Ok(bytes)) => {
                    self.screen.process(&bytes);
                    self.output.extend_from_slice(&bytes);
                }
                Ok(Err(error)) => {
                    bail!(
                        "PTY reader failed while waiting for raw {text:?}: {error}; output was {transcript:?}"
                    )
                }
                Err(RecvTimeoutError::Timeout) => {
                    bail!("timed out waiting for raw PTY text {text:?}; output was {transcript:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("PTY closed while waiting for raw {text:?}; output was {transcript:?}")
                }
            }
        }
    }

    fn wait_for_raw_text_since(
        &mut self,
        checkpoint: &PtyCheckpoint,
        text: &str,
    ) -> Result<String> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let bytes = &self.output[checkpoint.output_len..];
            if bytes
                .windows(text.len())
                .any(|window| window == text.as_bytes())
            {
                return Ok(String::from_utf8_lossy(bytes).to_string());
            }
            let transcript = String::from_utf8_lossy(bytes).to_string();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for raw PTY text {text:?} after checkpoint; output was {transcript:?}"
                );
            }
            match self.output_receiver.recv_timeout(remaining) {
                Ok(Ok(bytes)) => {
                    self.screen.process(&bytes);
                    self.output.extend_from_slice(&bytes);
                }
                Ok(Err(error)) => {
                    bail!("PTY reader failed while waiting for raw {text:?}: {error}")
                }
                Err(RecvTimeoutError::Timeout) => {
                    bail!("timed out waiting for raw PTY text {text:?} after checkpoint")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("PTY closed while waiting for raw {text:?} after checkpoint")
                }
            }
        }
    }

    fn wait_for_exit(&mut self) -> Result<portable_pty::ExitStatus> {
        match self.exit_receiver.recv_timeout(PTY_TIMEOUT) {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(anyhow::anyhow!("waiting for PTY child: {error}")),
            Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for PTY child exit"),
            Err(RecvTimeoutError::Disconnected) => bail!("PTY waiter exited without a status"),
        }
    }
}

fn synchronized_update_complete(output: &[u8]) -> bool {
    let begin = output
        .windows(BEGIN_SYNCHRONIZED_UPDATE.len())
        .rposition(|window| window == BEGIN_SYNCHRONIZED_UPDATE);
    let end = output
        .windows(END_SYNCHRONIZED_UPDATE.len())
        .rposition(|window| window == END_SYNCHRONIZED_UPDATE);
    begin.is_none_or(|begin| end.is_some_and(|end| end > begin))
}

impl Drop for PtyRunner {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

/// A Code process enclosed in a shell that reports terminal state after Code
/// exits. The marker makes terminal restoration and shell usability testable
/// without depending on the developer's controlling terminal.
struct CodeFixture {
    mock: MockAcp,
    child_pid_path: PathBuf,
    pty: PtyRunner,
}

impl CodeFixture {
    fn signal_child(&self, signal: &str) -> Result<()> {
        ensure!(
            matches!(signal, "INT" | "TERM" | "TSTP" | "CONT"),
            "unsupported fixture signal {signal:?}"
        );
        let deadline = Instant::now() + PTY_TIMEOUT;
        let pid = loop {
            match std::fs::read_to_string(&self.child_pid_path) {
                Ok(value) => match value.trim().parse::<u32>() {
                    Ok(pid) if pid > 0 => break pid,
                    Ok(_) | Err(_) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(_) | Err(_) => {
                        bail!(
                            "timed out reading a valid Code child PID from {}: {value:?}",
                            self.child_pid_path.display()
                        )
                    }
                },
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    bail!(
                        "timed out waiting for Code child PID file {}",
                        self.child_pid_path.display()
                    )
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "reading Code child PID file {}",
                            self.child_pid_path.display()
                        )
                    });
                }
            }
        };
        let status = Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .with_context(|| format!("delivering SIG{signal} to Code child {pid}"))?;
        ensure!(
            status.success(),
            "delivering SIG{signal} to Code child {pid} failed with {status}"
        );
        Ok(())
    }

    fn assert_terminal_restored(&mut self) -> Result<()> {
        let output = self.pty.wait_for_raw_text("__BITROUTER_PTY_SHELL_OK__")?;
        let start = output
            .find("__BITROUTER_PTY_TERM__|")
            .context("finding terminal-restoration marker")?;
        let marker = output[start..]
            .lines()
            .next()
            .context("reading terminal-restoration marker")?
            .trim_end_matches('\r');
        let mut fields = marker.split('|');
        let name = fields.next().context("reading marker name")?;
        let status = fields.next().context("reading Code exit status")?;
        let before = fields.next().context("reading initial terminal state")?;
        let after = fields.next().context("reading restored terminal state")?;

        ensure!(
            name == "__BITROUTER_PTY_TERM__",
            "unexpected terminal marker {name:?}"
        );
        ensure!(status == "0", "Code exited with status {status}");
        ensure!(
            terminal_configuration(before)? == terminal_configuration(after)?,
            "terminal state changed across Code: before={before:?}, after={after:?}"
        );
        ensure!(
            output.contains("__BITROUTER_PTY_SHELL_OK__"),
            "shell command after Code did not run"
        );
        Ok(())
    }
}

fn terminal_configuration(value: &str) -> Result<String> {
    #[cfg(target_os = "macos")]
    let value = {
        let mut local_fields = 0;
        let fields = value
            .split(':')
            .map(|field| {
                if let Some(flags) = field.strip_prefix("lflag=") {
                    local_fields += 1;
                    let flags = u64::from_str_radix(flags, 16)?;
                    // XNU's ICANON restoration adds transient PENDIN. All other
                    // stty fields and raw local flags remain part of the comparison.
                    Ok(format!("lflag={:x}", flags & !nix::libc::PENDIN))
                } else {
                    Ok(field.to_owned())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            local_fields == 1,
            "stty snapshot lacks one unambiguous lflag: {value}"
        );
        fields.join(":")
    };
    #[cfg(not(target_os = "macos"))]
    let value = value.to_owned();
    Ok(value)
}

fn shell_command(mock: &MockAcp) -> Result<CommandBuilder> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_bro"));
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    command.arg("-c");
    command.arg(
        "before=$(stty -g)\n\
         /bin/sh -c 'printf \"%s\\n\" \"$$\" > \"$BITROUTER_PTY_CHILD_PID\"; exec \"$@\"' bitrouter-pty-child \"$@\"\n\
         status=$?\n\
         after=$(stty -g)\n\
         printf '\\n__BITROUTER_PTY_TERM__|%s|%s|%s|\\n' \"$status\" \"$before\" \"$after\"\n\
         printf '__BITROUTER_PTY_SHELL_OK__\\n'\n\
         exit \"$status\"",
    );
    command.arg("bitrouter-pty-shell");
    command.arg(binary);
    command.env("PATH", inherited_path()?);
    command.env("HOME", &mock.home_path);
    command.env("BITROUTER_HOME", &mock.bitrouter_home_path);
    command.env("TMPDIR", &mock.temporary_path);
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    command.env("BITROUTER_PTY_SCENARIO", mock.scenario.name());
    command.env("BITROUTER_PTY_CAPTURE", &mock.capture_path);
    command.env("BITROUTER_PTY_CONTROL", &mock.control_path);
    command.env(
        "BITROUTER_PTY_CHILD_PID",
        mock._directory.path().join("code-child.pid"),
    );
    command.cwd(mock._directory.path());
    Ok(command)
}

fn inherited_path() -> Result<OsString> {
    std::env::var_os("PATH").context("the PTY fixture requires PATH to locate its configured agent")
}

fn shell_word(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("'{}'", path.replace('\'', "'\"'\"'"))
}

fn acp_fixture_executable() -> Result<PathBuf> {
    static BINARY: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| fixture_binary::build("pty_acp_fixture"))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn yaml_string(path: &Path) -> Result<String> {
    serde_json::to_string(path.to_string_lossy().as_ref()).context("encoding YAML path")
}

fn find_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = object.get("text").and_then(serde_json::Value::as_str)
            {
                return Some(text.to_string());
            }
            object.values().find_map(find_text)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_text),
        _ => None,
    }
}

#[test]
fn pty_runner_reads_writes_and_resizes_without_a_sleep() -> Result<()> {
    let mut command = CommandBuilder::new("/bin/sh");
    command.arg("-c");
    command.arg(
        "printf '__PTY_READY__\\n'\n\
         IFS= read -r line\n\
         printf '__PTY_GOT__:%s\\n' \"$line\"\n\
         stty size",
    );
    let mut pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
    let _ = pty.wait_for_text("__PTY_READY__")?;
    pty.resize(40, 16)?;
    pty.send(b"hello\r")?;
    let _ = pty.wait_for_text("__PTY_GOT__:hello")?;
    let output = pty.wait_for_text("16 40")?;
    ensure!(
        output.contains("16 40"),
        "resized PTY dimensions missing from {output:?}"
    );
    ensure!(
        pty.wait_for_exit()?.success(),
        "PTY probe child did not exit successfully"
    );
    Ok(())
}

#[test]
fn mock_acp_minimal_lifecycle_logs_exact_multiline_prompt_text() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let mut agent = mock.start()?;
    let initialize = agent.request(
        "initialize",
        "initialize",
        serde_json::json!({"protocolVersion": 1}),
    )?;
    ensure!(
        initialize["result"]["agentCapabilities"]
            .as_object()
            .is_some_and(|capabilities| capabilities.is_empty()),
        "minimal fixture unexpectedly advertised optional capabilities: {initialize}"
    );
    let session = agent.request(
        "new",
        "session/new",
        serde_json::json!({"cwd": "/", "mcpServers": []}),
    )?;
    ensure!(
        session["result"]["sessionId"] == "pty-native",
        "minimal fixture did not return its native id: {session}"
    );
    let loaded = agent.request(
        "load",
        "session/load",
        serde_json::json!({"sessionId": "pty-native", "cwd": "/", "mcpServers": []}),
    )?;
    ensure!(
        loaded["result"]["_meta"]["loadedBy"] == "pty-fixture",
        "load result was not fixture-authored: {loaded}"
    );

    let prompt = "  中文 👩‍💻\nsecond line  ";
    let completed = agent.request(
        "prompt",
        "session/prompt",
        serde_json::json!({
            "sessionId": "pty-native",
            "prompt": [{"type": "text", "text": prompt}],
        }),
    )?;
    ensure!(
        completed["result"]["stopReason"] == "end_turn",
        "normal fixture prompt did not settle: {completed}"
    );
    ensure!(
        mock.captured_prompt()?.as_deref() == Some(prompt),
        "fixture did not preserve the exact multiline prompt"
    );
    Ok(())
}

#[test]
fn mock_acp_models_delayed_cancellation_and_overlapping_permissions() -> Result<()> {
    let delayed = MockAcp::new(MockScenario::DelayedCancel)?;
    let mut cancelling = delayed.start()?;
    let _ = cancelling.request(
        "initialize",
        "initialize",
        serde_json::json!({"protocolVersion": 1}),
    )?;
    let _ = cancelling.request(
        "new",
        "session/new",
        serde_json::json!({"cwd": "/", "mcpServers": []}),
    )?;
    cancelling.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "prompt",
        "method": "session/prompt",
        "params": {"sessionId": "pty-native", "prompt": [{"type": "text", "text": "wait"}]},
    }))?;
    let _ = cancelling.wait_for("delayed cancellation update", |value| {
        value["params"]["update"]["content"]["text"] == "FXCN"
    })?;
    cancelling.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "cancel",
        "method": "session/cancel",
        "params": {"sessionId": "pty-native"},
    }))?;
    let cancelled = cancelling.wait_for("cancelled prompt", |value| {
        value.get("id").and_then(serde_json::Value::as_str) == Some("prompt")
    })?;
    ensure!(
        cancelled["result"]["stopReason"] == "cancelled",
        "delayed cancellation did not settle the original prompt: {cancelled}"
    );

    let permissions = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let mut answering = permissions.start()?;
    let _ = answering.request(
        "initialize",
        "initialize",
        serde_json::json!({"protocolVersion": 1}),
    )?;
    let _ = answering.request(
        "new",
        "session/new",
        serde_json::json!({"cwd": "/", "mcpServers": []}),
    )?;
    answering.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "permission-prompt",
        "method": "session/prompt",
        "params": {"sessionId": "pty-native", "prompt": [{"type": "text", "text": "ask"}]},
    }))?;
    let first = answering.wait_for("first permission", |value| {
        value.get("id").and_then(serde_json::Value::as_str) == Some("permission-1")
    })?;
    let second = answering.wait_for("second permission", |value| {
        value.get("id").and_then(serde_json::Value::as_str) == Some("permission-2")
    })?;
    ensure!(
        first["params"]["toolCall"]["title"] == "fixture permission 1"
            && second["params"]["toolCall"]["title"] == "fixture permission 2",
        "overlapping permission identities were not distinct"
    );
    for id in ["permission-1", "permission-2"] {
        answering.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"outcome": "selected"},
        }))?;
    }
    let completed = answering.wait_for("permission prompt settlement", |value| {
        value.get("id").and_then(serde_json::Value::as_str) == Some("permission-prompt")
    })?;
    ensure!(
        completed["result"]["stopReason"] == "end_turn",
        "permission fixture did not settle after both responses: {completed}"
    );
    Ok(())
}

#[test]
fn mock_acp_refusal_settles_the_original_prompt_without_replay() -> Result<()> {
    let mock = MockAcp::new(MockScenario::DelayedRefusal)?;
    let mut process = mock.start()?;
    process.request(
        "initialize",
        "initialize",
        serde_json::json!({"protocolVersion":1}),
    )?;
    process.request(
        "new",
        "session/new",
        serde_json::json!({"cwd":"/", "mcpServers":[]}),
    )?;
    process.send(serde_json::json!({"jsonrpc":"2.0","id":"refused-prompt","method":"session/prompt",
        "params":{"sessionId":"pty-native","prompt":[{"type":"text","text":"refuse this exact prompt"}]}}))?;
    process.wait_for("refusal fixture ready", |value| {
        value["params"]["update"]["content"]["text"] == "FXRF"
    })?;
    mock.stop_with_refusal()?;
    let refused = process.wait_for("original refused prompt", |value| {
        value["id"] == "refused-prompt"
    })?;
    ensure!(refused["result"]["stopReason"] == "refusal");
    ensure!(mock.captured_prompts()? == vec!["refuse this exact prompt".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn native_attached_backlog_preserves_all_events_across_resize_and_reconnect() -> Result<()> {
    use bitrouter::supervisor::{SessionCommand, SessionResponse, SessionScope};
    let mock = MockAcp::new(MockScenario::DetachedBacklog)?;
    let launched = mock
        .bro_command()?
        .args([
            "run",
            "stub",
            "produce detached backlog",
            "--direct",
            "--background",
            "--json",
        ])
        .output()
        .await?;
    ensure!(
        launched.status.success(),
        "backlog launch failed: {}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&launched.stdout)?;
    let run = report["agent_run_id"]
        .as_str()
        .context("backlog run ID missing")?
        .to_owned();
    let socket = mock.supervisor_socket().await?;
    let reader = bitrouter::supervisor::authorize(
        &socket,
        "backlog-observer",
        [SessionScope::Peek, SessionScope::Transcript]
            .into_iter()
            .collect(),
    )
    .await?;
    let mut command = shell_command(&mock)?;
    command.arg("agents");
    command.arg("attach");
    command.arg(&run);
    let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
    let mut manager = CodeFixture {
        child_pid_path: mock._directory.path().join("code-child.pid"),
        mock,
        pty,
    };
    manager.pty.wait_for_raw_text(ENTER_ALTERNATE_SCREEN)?;
    manager.pty.wait_for_text("FXBG")?;
    for (cols, rows) in [(40, 16), (CODE_COLUMNS, CODE_ROWS)] {
        let changed = manager.pty.checkpoint();
        manager.pty.resize(cols, rows)?;
        manager.pty.send(b"\x0c")?;
        manager.pty.wait_for_text_since(&changed, "FXBG")?;
    }
    manager.mock.release_backlog()?;
    let deadline = Instant::now() + PTY_TIMEOUT;
    let replay = loop {
        let SessionResponse::Events { replay } = bitrouter::supervisor::request(
            &socket,
            &reader,
            SessionCommand::Events {
                run_id: run.clone(),
                after_seq: 0,
            },
        )
        .await?
        else {
            bail!("backlog replay missing");
        };
        let encoded = serde_json::to_string(&replay.events)?;
        if encoded.contains("BG089") {
            break replay;
        }
        ensure!(
            Instant::now() < deadline,
            "backlog never reached retained history"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    ensure!(
        replay.history_complete
            && replay
                .events
                .windows(2)
                .all(|events| events[0].seq < events[1].seq)
    );
    let encoded = serde_json::to_string(&replay.events)?;
    for number in 0..90 {
        let marker = format!("BG{number:03}");
        ensure!(
            encoded.matches(&marker).count() == 1,
            "retained backlog repeated or omitted {marker}"
        );
    }
    manager.pty.send(b"\x1b[F")?;
    manager.pty.wait_for_text("BG089")?;
    manager.signal_child("TERM")?;
    manager.assert_terminal_restored()?;
    let mut command = shell_command(&manager.mock)?;
    command.arg("agents");
    command.arg("attach");
    command.arg(&run);
    manager.pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
    manager.pty.wait_for_raw_text(ENTER_ALTERNATE_SCREEN)?;
    manager.pty.send(b"\x1b[F")?;
    manager.pty.wait_for_text("BG089")?;
    manager.signal_child("TERM")?;
    manager.assert_terminal_restored()?;
    let SessionResponse::Events { replay: after } = bitrouter::supervisor::request(
        &socket,
        &reader,
        SessionCommand::Events {
            run_id: run.clone(),
            after_seq: 0,
        },
    )
    .await?
    else {
        bail!("reconnected replay missing");
    };
    let encoded = serde_json::to_string(&after.events)?;
    for number in 0..90 {
        let marker = format!("BG{number:03}");
        ensure!(
            encoded.matches(&marker).count() == 1,
            "reconnection repeated or omitted {marker}"
        );
    }
    ensure!(manager.mock.captured_prompts()? == vec!["produce detached backlog".to_owned()]);
    ensure!(
        manager
            .mock
            .supervised_runs()
            .await?
            .iter()
            .any(|snapshot| snapshot.run_id == run
                && snapshot.process == bitrouter::supervisor::ProcessState::Running
                && snapshot.lease.is_none()),
        "inspector exit stopped or retained control of backlog run"
    );
    manager.mock.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn background_cli_returns_before_settlement_and_run_survives_client_exit() -> Result<()> {
    let mock = MockAcp::new(MockScenario::DelayedNormal)?;
    let output = tokio::time::timeout(
        PTY_TIMEOUT,
        mock.bro_command()?
            .args([
                "run",
                "stub",
                "work after this client exits",
                "--direct",
                "--background",
                "--json",
            ])
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "background launch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let run_id = report
        .get("agent_run_id")
        .and_then(serde_json::Value::as_str)
        .context("successful background launch omitted attachable run ID")?;
    let runs = mock.supervised_runs().await?;
    let run = runs
        .iter()
        .find(|run| run.run_id == run_id)
        .context("accepted run missing after CLI exited")?;
    assert_eq!(run.process, bitrouter::supervisor::ProcessState::Running);
    assert_eq!(run.turn, bitrouter::supervisor::TurnState::Working);
    assert!(run.lease.is_none());
    mock.wait_for_request("session/prompt")?;
    mock.release()?;
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            let runs = mock.supervised_runs().await?;
            if runs.iter().any(|run| {
                run.run_id == run_id
                    && run.review == bitrouter::supervisor::ReviewState::Unread
                    && run.turn == bitrouter::supervisor::TurnState::Idle
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let stopped = tokio::time::timeout(
        PTY_TIMEOUT,
        mock.bro_command()?
            .args(["agents", "stop", run_id, "--json"])
            .output(),
    )
    .await??;
    ensure!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let removed = tokio::time::timeout(
        PTY_TIMEOUT,
        mock.bro_command()?
            .args(["agents", "remove", run_id, "--json"])
            .output(),
    )
    .await??;
    ensure!(
        removed.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(mock.supervised_runs().await?.is_empty());
    mock.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn standalone_agents_and_attach_restore_terminal_without_stopping_runs() -> Result<()> {
    for attach in [false, true] {
        let mock = MockAcp::new(MockScenario::Minimal)?;
        let launched = tokio::time::timeout(
            PTY_TIMEOUT,
            mock.bro_command()?
                .args([
                    "run",
                    "stub",
                    "standalone fixture",
                    "--background",
                    "--direct",
                    "--json",
                ])
                .output(),
        )
        .await??;
        ensure!(
            launched.status.success(),
            "standalone fixture launch failed: {}",
            String::from_utf8_lossy(&launched.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&launched.stdout)?;
        let run_id = report
            .get("agent_run_id")
            .and_then(serde_json::Value::as_str)
            .context("standalone fixture missing run ID")?
            .to_string();
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock)?;
        command.arg("agents");
        if attach {
            command.arg("attach");
            command.arg(&run_id);
        }
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        let mut manager = CodeFixture {
            mock,
            child_pid_path,
            pty,
        };
        let _ = manager.pty.wait_for_raw_text(ENTER_ALTERNATE_SCREEN)?;
        let _ = manager
            .pty
            .wait_for_text(if attach { "FXRP1" } else { "Agents ·" })?;
        let suspended = manager.pty.checkpoint();
        manager.signal_child("TSTP")?;
        let _ = manager
            .pty
            .wait_for_raw_text_since(&suspended, LEAVE_ALTERNATE_SCREEN)?;
        let resumed = manager.pty.checkpoint();
        manager.signal_child("CONT")?;
        let _ = manager
            .pty
            .wait_for_raw_text_since(&resumed, ENTER_ALTERNATE_SCREEN)?;
        manager.signal_child("TERM")?;
        manager.assert_terminal_restored()?;
        let runs = manager.mock.supervised_runs().await?;
        ensure!(
            runs.iter().any(|run| run.run_id == run_id
                && run.process == bitrouter::supervisor::ProcessState::Running
                && run.lease.is_none()),
            "manager exit stopped or retained control of the background run"
        );
        manager.mock.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn remote_cli_read_actions_preserve_authentication_and_never_start_local_work() -> Result<()>
{
    let remote = RemoteFixture::new(RemoteScenario::Healthy)?;
    for (args, method, path, markers) in [
        (
            vec!["status"],
            "GET",
            "/control/v1/status",
            vec!["REMOTE_A12_STATUS", "REMOTE_A12_PROVIDER"],
        ),
        (
            vec!["models"],
            "GET",
            "/control/v1/models",
            vec!["REMOTE_A12_MODEL", "REMOTE_A12_PROVIDER"],
        ),
        (
            vec!["requests"],
            "GET",
            "/control/v1/requests",
            vec!["REMOTE_A12_SCOPE", "REMOTE_A12_WINDOW"],
        ),
        (
            vec!["route", "REMOTE_A12_MODEL"],
            "POST",
            "/control/v1/route/preview",
            vec!["REMOTE_A12_ROUTE_RESULT", "remote-a12-service"],
        ),
    ] {
        let output =
            tokio::time::timeout(PTY_TIMEOUT, remote.bro_command()?.args(args).output()).await??;
        ensure!(
            output.status.success(),
            "remote read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            !output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b),
            "remote CLI emitted terminal control bytes"
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let text = report.to_string();
        ensure!(
            markers.iter().all(|marker| text.contains(marker)),
            "remote fixture data missing: {text}"
        );
        let capabilities = remote.wait_for_request("GET", "/control/v1/capabilities")?;
        remote.assert_bearer(&capabilities)?;
        let read = remote.wait_for_request(method, path)?;
        remote.assert_bearer(&read)?;
        if method == "POST" {
            let input: serde_json::Value = serde_json::from_str(&read.body)?;
            assert_eq!(input["model"], "REMOTE_A12_MODEL");
        }
    }
    assert!(!remote.bitrouter_home_path.join("bitrouter.sock").exists());
    assert!(!remote.bitrouter_home_path.join("bitrouter.pid").exists());
    assert!(!remote.bitrouter_home_path.join("bitrouter.yaml").exists());
    ensure!(
        matches!(remote.requests.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "read CLI issued extra remote operations"
    );
    Ok(())
}

#[tokio::test]
async fn remote_cli_failure_keeps_the_remote_error_without_local_model_fallback() -> Result<()> {
    let remote = RemoteFixture::new(RemoteScenario::ModelsFailure)?;
    let output = tokio::time::timeout(PTY_TIMEOUT, remote.bro_command()?.args(["models"]).output())
        .await??;
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        message.contains("REMOTE_A12_MODELS_FAILURE"),
        "remote failure was lost: {message}"
    );
    ensure!(
        !message.contains("Listed from config")
            && !message.contains("no daemon answered")
            && !message.contains("(no routable models)"),
        "remote error fell back to local data: {message}"
    );
    let capabilities = remote.wait_for_request("GET", "/control/v1/capabilities")?;
    remote.assert_bearer(&capabilities)?;
    let models = remote.wait_for_request("GET", "/control/v1/models")?;
    remote.assert_bearer(&models)?;
    assert!(!remote.bitrouter_home_path.join("bitrouter.sock").exists());
    assert!(!remote.bitrouter_home_path.join("bitrouter.pid").exists());
    ensure!(matches!(
        remote.requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn explicit_socket_status_is_readonly_without_starting_an_agent() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let socket = mock.supervisor_socket().await?;
    let mut server = mock
        .bro_command()?
        .args(["serve", "--config"])
        .arg(&mock.config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            if bitrouter::daemon::probe_status(&socket).await?.is_some() {
                break Ok::<_, anyhow::Error>(());
            }
            ensure!(
                server.try_wait()?.is_none(),
                "fixture daemon exited before readiness"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    let output = tokio::time::timeout(
        PTY_TIMEOUT,
        mock.bro_command()?
            .args(["status", "--socket"])
            .arg(&socket)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "explicit socket read failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["running"], true);
    assert!(mock.supervised_runs().await?.is_empty());
    assert!(mock.captured_request("initialize")?.is_none());
    assert!(mock.captured_prompts()?.is_empty());
    ensure!(!output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b));
    match bitrouter::daemon::send_command(&socket, &bitrouter::daemon::DaemonCommand::Stop).await? {
        bitrouter::daemon::DaemonResponse::Ok => {}
        response => bail!("fixture daemon stop failed: {response:?}"),
    }
    ensure!(
        tokio::time::timeout(PTY_TIMEOUT, server.wait())
            .await??
            .success()
    );
    Ok(())
}
