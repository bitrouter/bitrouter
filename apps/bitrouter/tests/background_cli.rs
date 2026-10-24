//! CLI and protocol business regressions migrated from the retired TUI harness.

#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
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
const PTY_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_FIXTURE_TOKEN: &str = "remote-a12-fixture-token-0123456789";
const MOCK_DAEMON_CONFIG: &str =
    "server:\n  listen: 127.0.0.1:0\n  skip_auth: true\nregistry:\n  enabled: false\n";
#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;
#[path = "background_cli/lifecycle.rs"]
mod lifecycle;

#[derive(Debug, Clone, Copy)]
enum MockScenario {
    Minimal,
    DelayedCancel,
    DelayedNormal,
    OverlappingPermissions,
    SessionLifecycle,
}

impl MockScenario {
    fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::DelayedCancel => "delayed-cancel",
            Self::DelayedNormal => "delayed-normal",
            Self::OverlappingPermissions => "overlapping-permissions",
            Self::SessionLifecycle => "session-lifecycle",
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
        let mut child = command.spawn().context("starting mock ACP process")?;
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

    fn disconnect(&self) -> Result<()> {
        self.control("disconnect")
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

fn inherited_path() -> Result<OsString> {
    std::env::var_os("PATH").context("the PTY fixture requires PATH to locate its configured agent")
}

fn acp_fixture_executable() -> Result<PathBuf> {
    static BINARY: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| example_binary::build("bitrouter", "pty_acp_fixture"))
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
    Ok(())
}

impl RemoteFixture {
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(["--context", "fixture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home_path)
            .env("BITROUTER_HOME", &self.bitrouter_home_path)
            .env("TMPDIR", &self.temporary_path)
            .env("NO_COLOR", "1")
            .env("BITROUTER_A12_TOKEN", REMOTE_FIXTURE_TOKEN)
            .current_dir(self._directory.path())
            .kill_on_drop(true);
        command
    }
}

#[tokio::test]
async fn remote_cli_reads_use_authenticated_remote_actions() -> Result<()> {
    let remote = RemoteFixture::new(RemoteScenario::Healthy)?;
    for (args, method, target, marker) in [
        (
            vec!["status"],
            "GET",
            "/control/v1/status",
            "REMOTE_A12_STATUS",
        ),
        (
            vec!["models"],
            "GET",
            "/control/v1/models",
            "REMOTE_A12_MODEL",
        ),
        (
            vec!["requests"],
            "GET",
            "/control/v1/requests",
            "REMOTE_A12_SCOPE",
        ),
        (
            vec!["route", "REMOTE_A12_MODEL"],
            "POST",
            "/control/v1/route/preview",
            "REMOTE_A12_ROUTE_RESULT",
        ),
    ] {
        let output =
            tokio::time::timeout(PTY_TIMEOUT, remote.command().args(args).output()).await??;
        ensure!(
            output.status.success(),
            "remote CLI failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        ensure!(
            report.to_string().contains(marker),
            "remote data missing: {report}"
        );
        remote.assert_bearer(&remote.wait_for_request("GET", "/control/v1/capabilities")?)?;
        let request = remote.wait_for_request(method, target)?;
        remote.assert_bearer(&request)?;
        if method == "POST" {
            ensure!(
                request.body.contains("REMOTE_A12_MODEL"),
                "route preview lost the selected model: {request:?}"
            );
        }
    }
    ensure!(!remote.bitrouter_home_path.join("bitrouter.pid").exists());
    Ok(())
}

#[tokio::test]
async fn remote_cli_errors_do_not_fall_back_to_local_models() -> Result<()> {
    let remote = RemoteFixture::new(RemoteScenario::ModelsFailure)?;
    let output = remote.command().args(["models"]).output().await?;
    ensure!(!output.status.success());
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        error.contains("REMOTE_A12_MODELS_FAILURE"),
        "remote failure missing: {error}"
    );
    for wrong in [
        "(no routable models)",
        "Listed from config",
        "no daemon answered",
    ] {
        ensure!(
            !error.contains(wrong),
            "remote failure became local data: {error}"
        );
    }
    remote.assert_bearer(&remote.wait_for_request("GET", "/control/v1/capabilities")?)?;
    remote.assert_bearer(&remote.wait_for_request("GET", "/control/v1/models")?)?;
    Ok(())
}
