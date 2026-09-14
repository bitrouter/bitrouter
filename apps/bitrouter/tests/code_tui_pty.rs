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
use std::os::unix::fs::PermissionsExt;
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

#[path = "code_tui_pty/evolution.rs"]
mod evolution;

/// A deliberately small ACP agent. Its responses are protocol-shaped JSON, not
/// terminal snapshots, so test failures describe lifecycle behavior instead of
/// an incidental escape-sequence layout.
const MOCK_ACP_SCRIPT: &str = r###"
import json
import os
import socket
import sys
import threading

scenario = os.environ.get("BITROUTER_PTY_SCENARIO", "minimal")
capture_path = os.environ["BITROUTER_PTY_CAPTURE"]
control_path = os.environ.get("BITROUTER_PTY_CONTROL")
pending_prompt = None
permission_ids = set()
prompt_count = 0
current_session_id = "pty-native"
state_lock = threading.Lock()
output_lock = threading.Lock()

def send(value):
    with output_lock:
        sys.stdout.write(json.dumps(value, separators=(",", ":"), ensure_ascii=False) + "\n")
        sys.stdout.flush()

def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})

def update(text, session_id=None):
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id or current_session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    })

def settings_options(current):
    return [{
        "id": "a12-setting",
        "name": "A12 setting",
        "description": "Fixture setting for confirmation coverage",
        "type": "select",
        "currentValue": current,
        "options": [
            {"value": "a12-old", "name": "A12 old"},
            {"value": "a12-confirmed", "name": "A12 confirmed"},
        ],
    }]

def finish(prompt_id, reason="end_turn"):
    respond(prompt_id, {"stopReason": reason})

def take_pending():
    global pending_prompt
    with state_lock:
        prompt = pending_prompt
        pending_prompt = None
    return prompt

def settle_pending(reason, text):
    prompt = take_pending()
    if prompt is not None:
        update(text)
        finish(prompt, reason)

def emit_permissions():
    global permission_ids
    with state_lock:
        permission_ids = {"permission-1", "permission-2"}
    for number in (1, 2):
        send({
            "jsonrpc": "2.0",
            "id": "permission-" + str(number),
            "method": "session/request_permission",
            "params": {
                "sessionId": "pty-native",
                "toolCall": {
                    "toolCallId": "tool-" + str(number),
                    "title": "fixture permission " + str(number),
                },
                "options": [
                    {
                        "optionId": "allow-" + str(number),
                        "name": "Allow fixture " + str(number),
                        "kind": "allow_once",
                    },
                    {
                        "optionId": "deny-" + str(number),
                        "name": "Deny fixture " + str(number),
                        "kind": "reject_once",
                    },
                ],
            },
        })

def control_loop(server):
    while True:
        connection, _ = server.accept()
        try:
            command = connection.recv(128).decode("utf-8").strip()
            if command == "release":
                settle_pending("end_turn", "FXRL")
            elif command == "refusal":
                settle_pending("refusal", "FXST")
            elif command == "permission":
                emit_permissions()
            elif command == "backlog":
                prompt = take_pending()
                if prompt is not None:
                    for number in range(90):
                        update("BG" + str(number).zfill(3) + "\n")
                    finish(prompt)
            connection.sendall(b"ok\n")
            if command == "disconnect":
                os._exit(0)
        finally:
            connection.close()

if control_path:
    try:
        os.unlink(control_path)
    except FileNotFoundError:
        pass
    control_server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    control_server.bind(control_path)
    control_server.listen()
    threading.Thread(target=control_loop, args=(control_server,), daemon=True).start()

with open(capture_path, "ab") as capture:
    for raw in sys.stdin.buffer:
        capture.write(raw)
        capture.flush()
        message = json.loads(raw)
        method = message.get("method")
        request_id = message.get("id")

        if method == "initialize":
            capabilities = {}
            if scenario in ("settings-confirmed", "settings-failure", "session-lifecycle"):
                capabilities = {
                    "loadSession": True,
                    "sessionCapabilities": {"list": {}, "resume": {}},
                }
            respond(request_id, {
                "protocolVersion": 1,
                "agentCapabilities": capabilities,
                "agentInfo": {"name": "pty-minimal", "version": "1"},
            })
        elif method == "session/new":
            if scenario == "fresh-sessions":
                current_session_id = "pty-fresh-" + str(os.getpid())
            if scenario in ("settings-confirmed", "settings-failure"):
                respond(request_id, {
                    "sessionId": "pty-native",
                    "configOptions": settings_options("a12-old"),
                    "_meta": {"agentSessionId": "agent-a12-settings"},
                })
                update("FXSET")
            else:
                respond(request_id, {"sessionId": current_session_id})
            update("FXRD " + current_session_id if scenario == "fresh-sessions" else "FXRD")
        elif method == "session/load":
            if scenario == "session-lifecycle":
                session_id = message["params"].get("sessionId", "native-a12-load")
                update("FXLOAD", session_id)
                respond(request_id, {
                    "configOptions": settings_options("a12-old"),
                    "_meta": {"agentSessionId": "agent-a12-load"},
                })
            else:
                respond(request_id, {"_meta": {"loadedBy": "pty-fixture"}})
        elif method == "session/resume":
            respond(request_id, {
                "configOptions": settings_options("a12-old"),
                "_meta": {"agentSessionId": "agent-a12-resume"},
            })
        elif method == "session/set_config_option":
            if scenario == "settings-confirmed":
                respond(request_id, {"configOptions": settings_options("a12-confirmed")})
            elif scenario == "settings-failure":
                send({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32000, "message": "FXSF"},
                })
            else:
                respond(request_id, {"configOptions": []})
        elif method == "session/prompt":
            prompt_count += 1
            if scenario == "fresh-sessions" and "wait-for-timeout" in json.dumps(message):
                with state_lock:
                    pending_prompt = request_id
                update("FXWAIT")
            elif scenario == "delayed-cancel":
                with state_lock:
                    pending_prompt = request_id
                update("FXCN")
            elif scenario == "delayed-normal":
                with state_lock:
                    pending_prompt = request_id
                update("FXW" + str(prompt_count))
            elif scenario == "delayed-refusal":
                with state_lock:
                    pending_prompt = request_id
                update("FXRF")
            elif scenario == "overlapping-permissions":
                with state_lock:
                    pending_prompt = request_id
                emit_permissions()
            elif scenario == "permission-during-inspector":
                with state_lock:
                    pending_prompt = request_id
                update("FXPD")
            elif scenario == "detached-backlog":
                with state_lock:
                    pending_prompt = request_id
                update("FXBG")
            else:
                update("FXRP" + str(prompt_count))
                finish(request_id)
        elif method == "session/cancel":
            if request_id is not None:
                respond(request_id, {})
            settle_pending("cancelled", "FXCS")
        elif request_id in permission_ids:
            with state_lock:
                permission_ids.remove(request_id)
                should_finish = not permission_ids and pending_prompt is not None
            if should_finish:
                prompt = take_pending()
                if prompt is not None:
                    finish(prompt)
"###;

/// The mode an isolated mock agent should exercise.
#[derive(Debug, Clone, Copy)]
enum MockScenario {
    Minimal,
    DelayedCancel,
    DelayedNormal,
    DelayedRefusal,
    OverlappingPermissions,
    PermissionDuringInspector,
    DetachedBacklog,
    SettingsConfirmed,
    SettingsFailure,
    SessionLifecycle,
    FreshSessions,
}

impl MockScenario {
    fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::DelayedCancel => "delayed-cancel",
            Self::DelayedNormal => "delayed-normal",
            Self::DelayedRefusal => "delayed-refusal",
            Self::OverlappingPermissions => "overlapping-permissions",
            Self::PermissionDuringInspector => "permission-during-inspector",
            Self::DetachedBacklog => "detached-backlog",
            Self::SettingsConfirmed => "settings-confirmed",
            Self::SettingsFailure => "settings-failure",
            Self::SessionLifecycle => "session-lifecycle",
            Self::FreshSessions => "fresh-sessions",
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
    python: PathBuf,
    script_path: PathBuf,
    scenario: MockScenario,
}

impl MockAcp {
    fn new(scenario: MockScenario) -> Result<Self> {
        let directory = tempfile::tempdir().context("creating PTY fixture directory")?;
        let python = python_executable()?;
        let script_path = directory.path().join("mock-acp.py");
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

        std::fs::write(&script_path, MOCK_ACP_SCRIPT).context("writing mock ACP script")?;
        let command = yaml_string(&python)?;
        let script = yaml_string(&script_path)?;
        let capture = yaml_string(&capture_path)?;
        let control = yaml_string(&control_path)?;
        let config = format!(
            "agents:\n\
             \u{20} stub:\n\
             \u{20} \u{20} name: stub\n\
             \u{20} \u{20} transport:\n\
             \u{20} \u{20} \u{20} type: stdio\n\
             \u{20} \u{20} \u{20} command: {command}\n\
             \u{20} \u{20} \u{20} args: [{script}]\n\
             \u{20} \u{20} \u{20} env:\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_SCENARIO: {}\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_CAPTURE: {capture}\n\
             \u{20} \u{20} \u{20} \u{20} BITROUTER_PTY_CONTROL: {control}\n",
            scenario.name()
        );
        std::fs::write(&config_path, config).context("writing mock ACP config")?;

        Ok(Self {
            _directory: directory,
            config_path,
            capture_path,
            control_path,
            home_path,
            bitrouter_home_path,
            temporary_path,
            python,
            script_path,
            scenario,
        })
    }

    fn start(&self) -> Result<MockProcess> {
        let mut command = Command::new(&self.python);
        command
            .arg(&self.script_path)
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

    fn wait_for_permission_outcomes(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let outcomes = self.captured_permission_outcomes()?;
            if outcomes.len() >= 2 {
                return Ok(outcomes);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for both pending permission outcomes; received {outcomes:?}"
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

    fn request_permissions(&self) -> Result<()> {
        self.control("permission")
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

    fn visual_editor(&self, text: &str) -> Result<VisualEditor> {
        let content_path = self._directory.path().join("external-editor-content");
        let script_path = self._directory.path().join("external-editor.sh");
        std::fs::write(&content_path, text).context("writing external editor fixture text")?;
        std::fs::write(
            &script_path,
            "#!/bin/sh\ncat \"$BITROUTER_PTY_VISUAL_CONTENT\" > \"$1\"\n",
        )
        .context("writing external editor fixture")?;
        let mut permissions = std::fs::metadata(&script_path)
            .context("reading external editor fixture permissions")?
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions)
            .context("making external editor fixture executable")?;
        Ok(VisualEditor {
            command: shell_word(&script_path),
            content_path,
        })
    }

    fn failing_visual_editor(&self) -> Result<VisualEditor> {
        let content_path = self._directory.path().join("external-editor-content");
        let script_path = self._directory.path().join("external-editor-fails.sh");
        std::fs::write(&content_path, "").context("writing failed external editor fixture text")?;
        std::fs::write(&script_path, "#!/bin/sh\nexit 42\n")
            .context("writing failed external editor fixture")?;
        let mut permissions = std::fs::metadata(&script_path)
            .context("reading failed external editor fixture permissions")?
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions)
            .context("making failed external editor fixture executable")?;
        Ok(VisualEditor {
            command: shell_word(&script_path),
            content_path,
        })
    }
}

struct VisualEditor {
    command: String,
    content_path: PathBuf,
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

    fn paste(&mut self, text: &str) -> Result<()> {
        self.send(b"\x1b[200~")?;
        self.send(text.as_bytes())?;
        self.send(b"\x1b[201~")
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
                    bail!("timed out waiting for visible PTY text {text:?}; screen was {screen:?}")
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
    fn agent(scenario: MockScenario) -> Result<Self> {
        let mock = MockAcp::new(scenario)?;
        Self::agent_with_mock(mock, None)
    }

    fn agent_with_selection(
        scenario: MockScenario,
        selection_flag: &str,
        native_id: &str,
    ) -> Result<Self> {
        let mock = MockAcp::new(scenario)?;
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock, None)?;
        command.arg("code");
        command.arg("stub");
        command.arg("--direct");
        command.arg(selection_flag);
        command.arg(native_id);
        command.arg("--config");
        command.arg(&mock.config_path);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn agent_with_mock(mock: MockAcp, visual_editor: Option<&VisualEditor>) -> Result<Self> {
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock, visual_editor)?;
        command.arg("code");
        command.arg("stub");
        command.arg("--direct");
        command.arg("--config");
        command.arg(&mock.config_path);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn bare() -> Result<Self> {
        let mock = MockAcp::new(MockScenario::Minimal)?;
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock, None)?;
        command.arg("code");
        command.arg("--config");
        command.arg(&mock.config_path);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn chat(scenario: MockScenario) -> Result<Self> {
        let mock = MockAcp::new(scenario)?;
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock, None)?;
        command.arg("chat");
        command.arg("stub");
        command.arg("--direct");
        command.arg("--config");
        command.arg(&mock.config_path);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn tui_bare() -> Result<Self> {
        let mock = MockAcp::new(MockScenario::Minimal)?;
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let mut command = shell_command(&mock, None)?;
        command.arg("tui");
        command.arg("--config");
        command.arg(&mock.config_path);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn socket_only() -> Result<Self> {
        let mock = MockAcp::new(MockScenario::Minimal)?;
        let child_pid_path = mock._directory.path().join("code-child.pid");
        let socket = mock._directory.path().join("a12.sock");
        let mut command = shell_command(&mock, None)?;
        command.arg("code");
        command.arg("--config");
        command.arg(&mock.config_path);
        command.arg("--socket");
        command.arg(socket);
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self {
            mock,
            child_pid_path,
            pty,
        })
    }

    fn wait_for_agent_ready(&mut self) -> Result<()> {
        let _ = self.pty.wait_for_text("FXRD")?;
        Ok(())
    }

    fn close_to_composer(&mut self) -> Result<()> {
        let checkpoint = self.pty.checkpoint();
        self.pty.send(b"\x1b")?;
        let _ = self.pty.wait_for_text_since(&checkpoint, "┃ Message")?;
        Ok(())
    }

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
            before == after,
            "terminal state changed across Code: before={before:?}, after={after:?}"
        );
        ensure!(
            output.contains("__BITROUTER_PTY_SHELL_OK__"),
            "shell command after Code did not run"
        );
        Ok(())
    }
}

/// A remote `bro --context fixture code` process and its loopback target.
struct RemoteCodeFixture {
    remote: RemoteFixture,
    pty: PtyRunner,
}

impl RemoteCodeFixture {
    fn new(scenario: RemoteScenario) -> Result<Self> {
        let remote = RemoteFixture::new(scenario)?;
        let mut command = remote_shell_command(&remote)?;
        command.arg("--context");
        command.arg("fixture");
        command.arg("code");
        let pty = PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?;
        Ok(Self { remote, pty })
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
            before == after,
            "terminal state changed across Code: before={before:?}, after={after:?}"
        );
        ensure!(
            output.contains("__BITROUTER_PTY_SHELL_OK__"),
            "shell command after Code did not run"
        );
        Ok(())
    }
}

fn shell_command(mock: &MockAcp, visual_editor: Option<&VisualEditor>) -> Result<CommandBuilder> {
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
    if let Some(visual_editor) = visual_editor {
        command.env("VISUAL", &visual_editor.command);
        command.env("BITROUTER_PTY_VISUAL_CONTENT", &visual_editor.content_path);
    }
    command.cwd(mock._directory.path());
    Ok(command)
}

fn remote_shell_command(remote: &RemoteFixture) -> Result<CommandBuilder> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_bro"));
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    command.arg("-c");
    command.arg(
        "before=$(stty -g)\n\
         \"$@\"\n\
         status=$?\n\
         after=$(stty -g)\n\
         printf '\\n__BITROUTER_PTY_TERM__|%s|%s|%s|\\n' \"$status\" \"$before\" \"$after\"\n\
         printf '__BITROUTER_PTY_SHELL_OK__\\n'\n\
         exit \"$status\"",
    );
    command.arg("bitrouter-remote-pty-shell");
    command.arg(binary);
    command.env("PATH", inherited_path()?);
    command.env("HOME", &remote.home_path);
    command.env("BITROUTER_HOME", &remote.bitrouter_home_path);
    command.env("TMPDIR", &remote.temporary_path);
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    command.env("BITROUTER_A12_TOKEN", REMOTE_FIXTURE_TOKEN);
    command.cwd(remote._directory.path());
    Ok(command)
}

fn inherited_path() -> Result<OsString> {
    std::env::var_os("PATH").context("the PTY fixture requires PATH to locate its configured agent")
}

fn shell_word(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("'{}'", path.replace('\'', "'\"'\"'"))
}

fn python_executable() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("PYTHON") {
        candidates.push(configured);
    }
    candidates.extend([
        OsString::from("python3"),
        OsString::from("/usr/bin/python3"),
        OsString::from("python"),
    ]);

    for candidate in candidates {
        let available = Command::new(&candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if available {
            return Ok(PathBuf::from(candidate));
        }
    }
    bail!("a Python 3 executable is required for the isolated mock ACP fixture")
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
fn code_minimal_agent_preserves_multiline_prompt_history_and_terminal() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::Minimal)?;
    code.wait_for_agent_ready()?;
    let prompt = "  中文 👩‍💻\nsecond line  ";
    code.pty.paste(prompt)?;
    ensure!(
        code.mock.captured_prompt()?.is_none(),
        "bracketed paste submitted before Enter"
    );
    code.pty.send(b"\r")?;
    let output = code.pty.wait_for_text("Turn completed")?;
    ensure!(output.contains("FXRP1"), "agent reply was not rendered");
    ensure!(
        !output.contains("Home") && !output.contains("Agents") && !output.contains("Requests"),
        "legacy permanent navigation appeared in the Code transcript"
    );
    ensure!(
        code.mock.captured_prompts()? == vec![prompt.to_string()],
        "Code changed the prompt bytes before sending"
    );

    let history_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b[A")?;
    code.pty.send(b"\r")?;
    let _ = code.pty.wait_for_text_since(&history_checkpoint, "FXRP2")?;
    let _ = code
        .pty
        .wait_for_text_since(&history_checkpoint, "Turn completed")?;
    ensure!(
        code.mock.captured_prompts()? == vec![prompt.to_string(), prompt.to_string()],
        "up-arrow history changed the accepted prompt bytes"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_uses_normal_buffer_until_an_explicit_inspector() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::Minimal)?;
    code.wait_for_agent_ready()?;
    ensure!(
        !code
            .pty
            .output
            .windows(ENTER_ALTERNATE_SCREEN.len())
            .any(|window| window == ENTER_ALTERNATE_SCREEN.as_bytes()),
        "Code entered the alternate screen before an explicit inspector"
    );

    code.pty.send(b"inspect this turn\r")?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    let open = code.pty.checkpoint();
    code.pty.send(b"\x0f")?;
    let _ = code
        .pty
        .wait_for_raw_text_since(&open, ENTER_ALTERNATE_SCREEN)?;
    let _ = code.pty.wait_for_text_since(&open, "Full transcript")?;

    let close = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_raw_text_since(&close, LEAVE_ALTERNATE_SCREEN)?;
    let normal = code.pty.wait_for_text_since(&close, "Message")?;
    ensure!(
        normal.contains("inspect this turn"),
        "closing the inspector did not restore the normal-buffer transcript"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_detached_backlog_catches_up_once_after_resizes() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::DetachedBacklog)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"produce detached backlog\r")?;
    let _ = code.pty.wait_for_text("FXBG")?;
    code.pty.send(b"\x0f")?;
    let _ = code.pty.wait_for_text("Full transcript")?;

    let narrow = code.pty.checkpoint();
    code.pty.resize(40, 16)?;
    code.pty.send(b"\x0c")?;
    let _ = code.pty.wait_for_text_since(&narrow, "Full transcript")?;
    let wide = code.pty.checkpoint();
    code.pty.resize(CODE_COLUMNS, CODE_ROWS)?;
    code.pty.send(b"\x0c")?;
    let _ = code.pty.wait_for_text_since(&wide, "Full transcript")?;

    code.mock.release_backlog()?;
    let _ = code.pty.wait_for_text("BG010")?;
    code.pty.send(b"\x1b[F")?;
    let _ = code.pty.wait_for_text("BG089")?;
    let close = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code.pty.wait_for_text_since(&close, "Turn completed")?;
    let emitted = &code.pty.output[close.output_len..];
    for number in 0..90 {
        let marker = format!("BG{number:03}");
        let count = emitted
            .windows(marker.len())
            .filter(|window| *window == marker.as_bytes())
            .count();
        ensure!(
            count == 1,
            "detached catch-up emitted {marker} {count} times instead of once"
        );
    }
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn permission_arriving_in_inspector_needs_f2_and_fresh_selection() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::PermissionDuringInspector)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"permission while detached\r")?;
    let _ = code.pty.wait_for_text("FXPD")?;
    code.pty.send(b"\x0f")?;
    let _ = code.pty.wait_for_text("Full transcript")?;
    code.mock.request_permissions()?;
    let _ = code.pty.wait_for_text("Permission needed · F2 to review")?;

    let ignored = code.pty.checkpoint();
    code.pty.send(b"1\r\x0c")?;
    let _ = code
        .pty
        .wait_for_text_since(&ignored, "Permission needed · F2 to review")?;
    ensure!(
        code.mock.captured_permission_outcomes()?.is_empty(),
        "inspector keys answered a permission before explicit F2 focus"
    );

    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_text("Permission needed · F2 focuses oldest pending request")?;
    code.pty.send(b"\x1b[12~")?;
    let _ = code.pty.wait_for_text("Press a number to highlight")?;
    let no_selection = code.pty.checkpoint();
    code.pty.send(b"\r\x0c")?;
    let _ = code
        .pty
        .wait_for_text_since(&no_selection, "Press a number to highlight")?;
    ensure!(
        code.mock.captured_permission_outcomes()?.is_empty(),
        "permission focus reused buffered selection input"
    );

    code.pty.send(b"1\r")?;
    code.pty.send(b"\x1b[12~")?;
    let _ = code.pty.wait_for_text("Allow fixture 2")?;
    code.pty.send(b"1\r")?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    let outcomes = code.mock.wait_for_permission_outcomes()?;
    ensure!(
        outcomes.len() == 2,
        "permissions were not resolved exactly once"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_minimum_and_below_minimum_permission_paths_are_safe() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::OverlappingPermissions)?;
    code.wait_for_agent_ready()?;
    let resize_checkpoint = code.pty.checkpoint();
    code.pty.resize(40, 16)?;
    code.pty.send(b"\x0c")?;
    let _ = code
        .pty
        .wait_for_raw_text_since(&resize_checkpoint, "\x1b[?2026l")?;
    let resized = code.pty.checkpoint();
    code.pty.send("中文 👩‍💻 permission\r".as_bytes())?;
    let _ = code
        .pty
        .wait_for_text_since(&resized, "F2 permission (2)")?;

    let small = code.pty.checkpoint();
    code.pty.resize(30, 10)?;
    code.pty.send(b"\x0c")?;
    let raw_small = code
        .pty
        .wait_for_raw_text_since(&small, "Approval is disabled")?;
    ensure!(
        code.pty
            .screen
            .screen()
            .contents()
            .contains("Approval is disabled"),
        "below-minimum safety copy was emitted but not visible; raw={raw_small:?}; screen={:?}",
        code.pty.screen.screen().contents()
    );
    code.pty.send(b"\x1b[12~1\r")?;
    let guarded = code.pty.checkpoint();
    code.pty.send(b"\x0c")?;
    let _ = code
        .pty
        .wait_for_text_since(&guarded, "Approval is disabled")?;
    ensure!(
        code.mock.captured_permission_outcomes()?.is_empty(),
        "below-minimum input approved a permission"
    );
    code.pty.send(b"\x1b")?;
    let _ = code.pty.wait_for_text("fixture permission 2")?;
    let focus_second = code.pty.checkpoint();
    code.pty.send(b"\x1b[12~\x0c")?;
    let _ = code
        .pty
        .wait_for_raw_text_since(&focus_second, "\x1b[?2026h")?;
    code.pty.send(b"\x1b")?;
    let outcomes = code.mock.wait_for_permission_outcomes()?;
    ensure!(
        outcomes.len() == 2,
        "safe denial did not resolve both requests"
    );

    code.pty.resize(40, 16)?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    ensure!(
        code.mock.captured_prompts()? == vec!["中文 👩‍💻 permission".to_string()],
        "40×16 changed the submitted CJK/emoji prompt"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_external_editor_preserves_multiline_prompt_and_terminal() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let edited = "external-replacement 中文 👩‍💻\nsecond edited line  ";
    let visual_editor = mock.visual_editor(edited)?;
    let mut code = CodeFixture::agent_with_mock(mock, Some(&visual_editor))?;
    code.wait_for_agent_ready()?;
    code.pty.paste("draft that the editor replaces")?;
    let editor_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x07")?;
    let _ = code
        .pty
        .wait_for_text_since(&editor_checkpoint, "external-replacement")?;
    let submission_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\r")?;
    let _ = code
        .pty
        .wait_for_text_since(&submission_checkpoint, "FXRP1")?;
    let output = code
        .pty
        .wait_for_text_since(&submission_checkpoint, "Turn completed")?;
    ensure!(
        output.contains("FXRP1"),
        "external-editor prompt did not reach the minimal agent"
    );
    ensure!(
        code.mock.captured_prompts()? == vec![edited.to_string()],
        "external editor changed the submitted prompt bytes"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_failed_external_editor_retains_draft_and_recovers() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let visual_editor = mock.failing_visual_editor()?;
    let draft = "recoverable draft 中文 👩‍💻\nsecond draft line  ";
    let mut code = CodeFixture::agent_with_mock(mock, Some(&visual_editor))?;
    code.wait_for_agent_ready()?;
    code.pty.paste(draft)?;
    let failure_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x07")?;
    let failure = code
        .pty
        .wait_for_text_since(&failure_checkpoint, "External editor failed")?;
    ensure!(
        failure.contains("External editor failed"),
        "a nonzero external editor exit was not reported"
    );

    let submission_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\r")?;
    let _ = code
        .pty
        .wait_for_text_since(&submission_checkpoint, "FXRP1")?;
    let _ = code
        .pty
        .wait_for_text_since(&submission_checkpoint, "Turn completed")?;
    ensure!(
        code.mock.captured_prompts()? == vec![draft.to_string()],
        "a failed external editor did not retain the original draft for recovery"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

fn code_signal_exit_restores_terminal(signal: &str) -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::Minimal)?;
    code.wait_for_agent_ready()?;
    code.signal_child(signal)
        .with_context(|| format!("delivering SIG{signal} during an interactive Code session"))?;
    code.assert_terminal_restored()
        .with_context(|| format!("restoring the terminal after SIG{signal}"))
}

#[test]
fn code_sigterm_denies_pending_permissions_and_restores_terminal() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::OverlappingPermissions)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"permission interrupted by SIGTERM\r")?;
    let pending = code
        .pty
        .wait_for_text("Permission needed · F2 focuses oldest pending request (2)")?;
    ensure!(
        pending.contains("Permission needed · F2 focuses oldest pending request (2)"),
        "both pending permissions were not visible before SIGTERM"
    );

    code.signal_child("TERM")?;
    code.assert_terminal_restored()?;
    let outcomes = code.mock.wait_for_permission_outcomes()?;
    ensure!(
        outcomes.len() == 2,
        "SIGTERM produced duplicate or missing permission outcomes: {outcomes:?}"
    );
    for (request_id, option_id) in [("permission-1", "deny-1"), ("permission-2", "deny-2")] {
        let outcome = outcomes
            .iter()
            .find(|(observed_id, _)| observed_id == request_id)
            .map(|(_, outcome)| outcome)
            .with_context(|| format!("finding SIGTERM outcome for {request_id}"))?;
        ensure!(
            outcome.get("outcome").and_then(serde_json::Value::as_str) == Some("selected"),
            "SIGTERM must explicitly reject {request_id}, received {outcome}"
        );
        ensure!(
            outcome.get("optionId").and_then(serde_json::Value::as_str) == Some(option_id),
            "SIGTERM chose the wrong permission option for {request_id}: {outcome}"
        );
    }
    Ok(())
}

#[test]
fn code_sigint_restores_terminal_and_shell() -> Result<()> {
    code_signal_exit_restores_terminal("INT")
}

#[test]
fn code_sigtstp_restores_then_sigcont_reacquires_terminal() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::Minimal)?;
    code.wait_for_agent_ready()?;
    let suspended = code.pty.checkpoint();
    code.signal_child("TSTP")?;
    let _ = code
        .pty
        .wait_for_raw_text_since(&suspended, "\x1b[?2004l")?;

    let resumed = code.pty.checkpoint();
    code.signal_child("CONT")?;
    let _ = code.pty.wait_for_raw_text_since(&resumed, "\x1b[?2004h")?;
    let screen = code.pty.wait_for_text_since(&resumed, "Message")?;
    ensure!(
        screen.contains("FXRD"),
        "SIGCONT did not redraw the authoritative conversation"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_bare_entry_offers_selection_without_permanent_navigation() -> Result<()> {
    let mut bare = CodeFixture::bare()?;
    let selection = bare.pty.wait_for_text("claude-acp")?;
    ensure!(
        !selection.contains("Home")
            && !selection.contains("Agents")
            && !selection.contains("Requests"),
        "bare Code entry restored permanent navigation"
    );
    bare.close_to_composer()?;
    bare.pty.paste("draft before selection")?;
    let submit_checkpoint = bare.pty.checkpoint();
    bare.pty.send(b"\r")?;
    let bare_output = bare
        .pty
        .wait_for_text("Choose an agent before sending this draft")?;
    ensure!(
        !bare_output.contains("Home") && !bare_output.contains("Agents"),
        "bare Code entry restored permanent navigation"
    );
    let _ = bare
        .pty
        .wait_for_text_since(&submit_checkpoint, "claude-acp")?;
    bare.close_to_composer()?;
    let clear_checkpoint = bare.pty.checkpoint();
    bare.pty.send(b"\x03")?;
    let _ = bare
        .pty
        .wait_for_text_since(&clear_checkpoint, "Draft cleared")?;
    bare.pty.send(b"\x04")?;
    bare.assert_terminal_restored()
}

#[test]
fn code_hidden_chat_shares_palette_permissions_and_terminal_restoration() -> Result<()> {
    let mut code = CodeFixture::chat(MockScenario::OverlappingPermissions)?;
    code.wait_for_agent_ready()?;
    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let palette = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Choose agent")?;
    ensure!(
        !palette.contains("Home") && !palette.contains("Agents") && !palette.contains("Requests"),
        "hidden chat entry restored permanent navigation"
    );
    code.close_to_composer()?;

    code.pty.send(b"compatibility permission test\r")?;
    let _ = code
        .pty
        .wait_for_text("Permission needed · F2 focuses oldest pending request")?;
    let first_permission_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b[12~")?;
    let first_permission = code
        .pty
        .wait_for_text_since(&first_permission_checkpoint, "Allow fixture 1")?;
    ensure!(
        first_permission.contains("Deny fixture 1"),
        "hidden chat entry did not retain the first permission labels"
    );
    code.pty.send(b"1\r")?;
    let second_permission_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b[12~")?;
    let second_permission = code
        .pty
        .wait_for_text_since(&second_permission_checkpoint, "Allow fixture 2")?;
    ensure!(
        second_permission.contains("Deny fixture 2"),
        "hidden chat entry did not retain the second permission labels"
    );
    code.pty.send(b"1\r")?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    ensure!(
        code.mock.captured_prompts()? == vec!["compatibility permission test".to_string()],
        "hidden chat entry changed the submitted prompt"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_hidden_tui_bare_alias_uses_the_shared_empty_composer() -> Result<()> {
    let mut tui = CodeFixture::tui_bare()?;
    let selection = tui.pty.wait_for_text("claude-acp")?;
    ensure!(
        !selection.contains("Home")
            && !selection.contains("Agents")
            && !selection.contains("Requests"),
        "hidden tui entry restored permanent navigation"
    );
    tui.close_to_composer()?;
    tui.pty.send(b"\x04")?;
    tui.assert_terminal_restored()
}

#[test]
fn code_queues_fifo_work_and_drains_after_each_normal_settlement() -> Result<()> {
    let mut queued = CodeFixture::agent(MockScenario::DelayedNormal)?;
    queued.wait_for_agent_ready()?;
    queued.pty.send(b"first\r")?;
    let _ = queued.pty.wait_for_text("FXW1")?;
    queued.pty.paste("queued follow-up")?;
    queued.pty.send(b"\r")?;
    let _ = queued.pty.wait_for_text("Queued for the next turn (")?;

    queued.mock.release()?;
    let _ = queued.pty.wait_for_text("FXW2")?;
    ensure!(
        queued.mock.captured_prompts()?
            == vec!["first".to_string(), "queued follow-up".to_string()],
        "normal settlement did not dispatch exactly one queued prompt in FIFO order"
    );

    let final_settlement = queued.pty.checkpoint();
    queued.mock.release()?;
    let _ = queued
        .pty
        .wait_for_text_since(&final_settlement, "Turn completed")?;
    queued.pty.send(b"\x04")?;
    queued.assert_terminal_restored()
}

#[test]
fn code_cancellation_pauses_queue_without_replaying_uncertain_work() -> Result<()> {
    let mut queued = CodeFixture::agent(MockScenario::DelayedCancel)?;
    queued.wait_for_agent_ready()?;
    queued.pty.send(b"first\r")?;
    let _ = queued.pty.wait_for_text("FXCN")?;
    queued.pty.paste("queued follow-up")?;
    queued.pty.send(b"\r")?;
    let _ = queued.pty.wait_for_text("Queued for the next turn (")?;
    let cancellation_checkpoint = queued.pty.checkpoint();
    queued.pty.send(b"\x03")?;
    let _ = queued.pty.wait_for_text_since(
        &cancellation_checkpoint,
        "Cancelling · wait for the agent to settle",
    )?;
    let _ = queued
        .pty
        .wait_for_text_since(&cancellation_checkpoint, "Queue is paused.")?;
    ensure!(
        queued.mock.captured_prompts()? == vec!["first".to_string()],
        "cancelled turn replayed or drained queued work"
    );
    queued.pty.send(b"\x04")?;
    queued.assert_terminal_restored()
}

#[test]
fn code_abnormal_stop_pauses_queue_without_draining_it() -> Result<()> {
    let mut queued = CodeFixture::agent(MockScenario::DelayedRefusal)?;
    queued.wait_for_agent_ready()?;
    queued.pty.send(b"first\r")?;
    let _ = queued.pty.wait_for_text("FXRF")?;
    queued.pty.paste("queued after refusal")?;
    queued.pty.send(b"\r")?;
    let _ = queued.pty.wait_for_text("Queued for the next turn (")?;
    let refusal_checkpoint = queued.pty.checkpoint();
    queued.mock.stop_with_refusal()?;
    let _ = queued
        .pty
        .wait_for_text_since(&refusal_checkpoint, "Queue is paused.")?;
    ensure!(
        queued.mock.captured_prompts()? == vec!["first".to_string()],
        "abnormal stop drained queued work"
    );
    queued.pty.send(b"\x04")?;
    queued.assert_terminal_restored()
}

#[test]
fn code_overlapping_permissions_require_explicit_answers() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::OverlappingPermissions)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"permission test\r")?;
    let _ = code
        .pty
        .wait_for_text("Permission needed · F2 focuses oldest pending request")?;
    let first_panel_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b[12~")?;
    let panel = code
        .pty
        .wait_for_text_since(&first_panel_checkpoint, "Allow fixture 1")?;
    ensure!(
        panel.contains("Allow fixture 1") && panel.contains("Deny fixture 1"),
        "oldest permission labels were not preserved"
    );
    let _ = code.pty.wait_for_text("Press a number to highlight")?;
    let _ = code.pty.wait_for_text("Enter confirms")?;
    code.pty.send(b"1\r")?;
    let second_panel_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b[12~")?;
    let second_panel = code
        .pty
        .wait_for_text_since(&second_panel_checkpoint, "Allow fixture 2")?;
    ensure!(
        second_panel.contains("Allow fixture 2") && second_panel.contains("Deny fixture 2"),
        "second overlapping permission lost its identity or labels"
    );
    code.pty.send(b"1\r")?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    ensure!(
        code.mock.captured_prompts()? == vec!["permission test".to_string()],
        "permission answers started an unexpected extra prompt"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_remote_operations_use_authenticated_remote_read_actions() -> Result<()> {
    let mut code = RemoteCodeFixture::new(RemoteScenario::Healthy)?;
    let root = code.pty.wait_for_text("REMOTE_A12_STATUS")?;
    ensure!(
        root.contains("Remote operations") && root.contains("read-only"),
        "remote target and read-only scope were not visible: {root:?}"
    );
    ensure!(
        !root.contains("Message"),
        "remote root exposed an enabled coding composer: {root:?}"
    );
    let capabilities = code
        .remote
        .wait_for_request("GET", "/control/v1/capabilities")?;
    code.remote.assert_bearer(&capabilities)?;
    let status = code.remote.wait_for_request("GET", "/control/v1/status")?;
    code.remote.assert_bearer(&status)?;

    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let palette = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Routable models")?;
    ensure!(
        palette.contains("Status")
            && palette.contains("Host requests")
            && palette.contains("Route preview")
            && palette.contains("Providers"),
        "remote palette omitted a supported read action: {palette:?}"
    );
    ensure!(
        !palette.contains("Choose agent")
            && !palette.contains("Agent settings")
            && !palette.contains("Session route"),
        "remote palette offered ACP execution or route mutation: {palette:?}"
    );
    code.pty.send(b"Telemetry")?;
    let _ = code.pty.wait_for_text("Telemetry")?;
    let palette_return = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_text_since(&palette_return, "Ctrl-P opens target actions")?;
    let models_palette = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&models_palette, "Routable models")?;
    code.pty.send(b"Routable models\r")?;
    let models = code.pty.wait_for_text("REMOTE_A12_MODEL")?;
    ensure!(
        models.contains("REMOTE_A12_PROVIDER"),
        "models inspector did not render remote fixture data: {models:?}"
    );
    let models_request = code.remote.wait_for_request("GET", "/control/v1/models")?;
    code.remote.assert_bearer(&models_request)?;
    let models_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_text_since(&models_checkpoint, "REMOTE_A12_STATUS")?;

    let requests_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&requests_checkpoint, "Host requests")?;
    code.pty.send(b"Host requests\r")?;
    let requests = code.pty.wait_for_text("REMOTE_A12_SCOPE")?;
    ensure!(
        requests.contains("REMOTE_A12_WINDOW"),
        "requests inspector did not render remote fixture data: {requests:?}"
    );
    let requests_request = code
        .remote
        .wait_for_request("GET", "/control/v1/requests")?;
    code.remote.assert_bearer(&requests_request)?;
    let requests_return = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_text_since(&requests_return, "REMOTE_A12_STATUS")?;

    let preview_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&preview_checkpoint, "Route preview")?;
    code.pty.send(b"Route preview\r")?;
    let model_selector_checkpoint = code.pty.checkpoint();
    code.pty.send(b"REMOTE_A12_MODEL")?;
    let model_selector = code
        .pty
        .wait_for_text_since(&model_selector_checkpoint, "Model to preview")?;
    ensure!(
        model_selector.contains("Route preview"),
        "route preview did not request a model: {model_selector:?}"
    );
    code.pty.send(b"\r")?;
    let route = code.pty.wait_for_text("REMOTE_A12_ROUTE_RESULT")?;
    ensure!(
        route.contains("remote-a12-service"),
        "route preview did not render the remote fixture result: {route:?}"
    );
    let route_request = code
        .remote
        .wait_for_request("POST", "/control/v1/route/preview")?;
    code.remote.assert_bearer(&route_request)?;
    ensure!(
        route_request.body.contains("REMOTE_A12_MODEL"),
        "route preview request lost the selected model: {route_request:?}"
    );

    let route_return = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code
        .pty
        .wait_for_text_since(&route_return, "REMOTE_A12_STATUS")?;
    code.pty.send(b"\x1b")?;
    code.assert_terminal_restored()
}

#[test]
fn code_remote_errors_do_not_fall_back_to_local_models() -> Result<()> {
    let mut code = RemoteCodeFixture::new(RemoteScenario::ModelsFailure)?;
    let _ = code.pty.wait_for_text("REMOTE_A12_STATUS")?;
    let capabilities = code
        .remote
        .wait_for_request("GET", "/control/v1/capabilities")?;
    code.remote.assert_bearer(&capabilities)?;
    let status = code.remote.wait_for_request("GET", "/control/v1/status")?;
    code.remote.assert_bearer(&status)?;

    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Routable models")?;
    code.pty.send(b"Routable models\r")?;
    let failure = code.pty.wait_for_text("REMOTE_A12_MODELS_FAILURE")?;
    ensure!(
        !failure.contains("(no routable models)")
            && !failure.contains("Listed from config")
            && !failure.contains("no daemon answered"),
        "remote models failure fell back to local catalog data: {failure:?}"
    );
    let models = code.remote.wait_for_request("GET", "/control/v1/models")?;
    code.remote.assert_bearer(&models)?;
    let failure_return_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x03")?;
    let _ = code
        .pty
        .wait_for_text_since(&failure_return_checkpoint, "REMOTE_A12_STATUS")?;
    code.pty.send(b"\x03")?;
    code.assert_terminal_restored()
}

#[test]
fn code_socket_operations_remain_read_only_without_starting_an_agent() -> Result<()> {
    let mut code = CodeFixture::socket_only()?;
    let root = code.pty.wait_for_text("a12.sock")?;
    ensure!(
        root.contains("Local operations")
            && root.contains("read-only")
            && !root.contains("Message"),
        "socket-only entry did not show its read-only scope: {root:?}"
    );
    ensure!(
        code.mock.captured_prompt()?.is_none(),
        "socket-only operations unexpectedly started an ACP agent"
    );
    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let palette = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Routable models")?;
    ensure!(
        !palette.contains("Choose agent")
            && !palette.contains("Agent settings")
            && !palette.contains("Session route"),
        "socket-only operations offered ACP execution or route mutation: {palette:?}"
    );
    let palette_return_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x03")?;
    let _ = code
        .pty
        .wait_for_text_since(&palette_return_checkpoint, "a12.sock")?;
    code.pty.send(b"\x03")?;
    code.assert_terminal_restored()
}

#[test]
fn code_agent_settings_apply_only_a_confirmed_configuration() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::SettingsConfirmed)?;
    code.wait_for_agent_ready()?;
    let _ = code.pty.wait_for_text("FXSET")?;
    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Agent settings")?;
    code.pty.send(b"Agent settings\r")?;
    let _ = code.pty.wait_for_text("A12 setting")?;
    code.pty.send(b"A12 setting\r")?;
    let _ = code.pty.wait_for_text("A12 confirmed")?;
    let confirmation_checkpoint = code.pty.checkpoint();
    code.pty.send(b"A12 confirmed\r")?;
    let _ = code
        .pty
        .wait_for_text_since(&confirmation_checkpoint, "Message")?;
    let request = code.mock.wait_for_request("session/set_config_option")?;
    ensure!(
        request["params"]["sessionId"] == "pty-native"
            && request["params"]["configId"] == "a12-setting"
            && request["params"]["value"] == "a12-confirmed",
        "setting request changed the native id or selected value: {request}"
    );

    let verify_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&verify_checkpoint, "Agent settings")?;
    code.pty.send(b"Agent settings\r")?;
    let retained = code.pty.wait_for_text("Current: a12-confirmed")?;
    ensure!(
        !retained.contains("Current: a12-old"),
        "confirmed setting still displayed the old value: {retained:?}"
    );
    code.close_to_composer()?;
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_failed_agent_setting_keeps_the_last_confirmed_value() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::SettingsFailure)?;
    code.wait_for_agent_ready()?;
    let _ = code.pty.wait_for_text("FXSET")?;
    let palette_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&palette_checkpoint, "Agent settings")?;
    code.pty.send(b"Agent settings\r")?;
    let _ = code.pty.wait_for_text("A12 setting")?;
    code.pty.send(b"A12 setting\r")?;
    let _ = code.pty.wait_for_text("A12 confirmed")?;
    let failure_checkpoint = code.pty.checkpoint();
    code.pty.send(b"A12 confirmed\r")?;
    let failure = code.pty.wait_for_text_since(&failure_checkpoint, "FXSF")?;
    ensure!(
        !failure.contains("a12-confirmed"),
        "failed setting was displayed as confirmed: {failure:?}"
    );
    let failure_return_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x1b")?;
    let _ = code.pty.wait_for_screen_inner(
        Some(&failure_return_checkpoint),
        "originating setting picker with restored selection and dismissed error",
        |screen| {
            screen.contains("Agent setting · A12 setting")
                && screen.contains("› A12 confirmed")
                && !screen.contains("FXSF")
        },
    )?;
    code.close_to_composer()?;
    let request = code.mock.wait_for_request("session/set_config_option")?;
    ensure!(
        request["params"]["sessionId"] == "pty-native"
            && request["params"]["configId"] == "a12-setting"
            && request["params"]["value"] == "a12-confirmed",
        "failed setting request changed the native id or selected value: {request}"
    );

    let verify_checkpoint = code.pty.checkpoint();
    code.pty.send(b"\x10")?;
    let _ = code
        .pty
        .wait_for_text_since(&verify_checkpoint, "Agent settings")?;
    code.pty.send(b"Agent settings\r")?;
    let retained = code.pty.wait_for_text("Current: a12-old")?;
    ensure!(
        !retained.contains("Current: a12-confirmed"),
        "failed setting replaced the last confirmed value: {retained:?}"
    );
    code.close_to_composer()?;
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_load_replays_history_while_resume_keeps_native_ids_distinct() -> Result<()> {
    let mut loaded = CodeFixture::agent_with_selection(
        MockScenario::SessionLifecycle,
        "--load",
        "native-a12-load",
    )?;
    let replay = loaded.pty.wait_for_text("FXLOAD")?;
    ensure!(
        replay.contains("FXLOAD"),
        "load did not retain the fixture's replayed history: {replay:?}"
    );
    let details_checkpoint = loaded.pty.checkpoint();
    loaded.pty.send(b"\x10")?;
    let _ = loaded
        .pty
        .wait_for_text_since(&details_checkpoint, "Session details")?;
    loaded.pty.send(b"Session details\r")?;
    let details = loaded
        .pty
        .wait_for_text("Native session: native-a12-load")?;
    ensure!(
        details.contains("Provider session: agent-a12-load"),
        "load details did not preserve the agent-owned id: {details:?}"
    );
    let load_request = loaded.mock.wait_for_request("session/load")?;
    ensure!(
        load_request["params"]["sessionId"] == "native-a12-load",
        "load request changed its native id: {load_request}"
    );
    loaded.close_to_composer()?;
    loaded.pty.send(b"\x04")?;
    loaded.assert_terminal_restored()?;

    let mut resumed = CodeFixture::agent_with_selection(
        MockScenario::SessionLifecycle,
        "--resume",
        "native-a12-resume",
    )?;
    let resumed_notice = resumed
        .pty
        .wait_for_text("Earlier history was not replayed")?;
    ensure!(
        !resumed_notice.contains("FXLOAD"),
        "resume incorrectly replayed load history: {resumed_notice:?}"
    );
    let resume_details_checkpoint = resumed.pty.checkpoint();
    resumed.pty.send(b"\x10")?;
    let _ = resumed
        .pty
        .wait_for_text_since(&resume_details_checkpoint, "Session details")?;
    resumed.pty.send(b"Session details\r")?;
    let resume_details = resumed
        .pty
        .wait_for_text("Native session: native-a12-resume")?;
    ensure!(
        resume_details.contains("Provider session: agent-a12-resume"),
        "resume details did not preserve the agent-owned id: {resume_details:?}"
    );
    let resume_request = resumed.mock.wait_for_request("session/resume")?;
    ensure!(
        resume_request["params"]["sessionId"] == "native-a12-resume",
        "resume request changed its native id: {resume_request}"
    );
    resumed.close_to_composer()?;
    resumed.pty.send(b"\x04")?;
    resumed.assert_terminal_restored()
}

#[test]
fn code_late_adapter_disconnect_preserves_terminal_cleanup() -> Result<()> {
    let mut code = CodeFixture::agent(MockScenario::Minimal)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"completed before disconnect\r")?;
    let _ = code.pty.wait_for_text("FXRP1")?;
    let _ = code.pty.wait_for_text("Turn completed")?;
    let disconnect_checkpoint = code.pty.checkpoint();
    code.mock.disconnect()?;
    let disconnected = code
        .pty
        .wait_for_text_since(&disconnect_checkpoint, "disconnected · adapter closed")?;
    ensure!(
        disconnected.contains("FXRP1"),
        "late adapter disconnect discarded the completed transcript: {disconnected:?}"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}
