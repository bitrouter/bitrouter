//! Authenticated remote service administration through the public CLI.

#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError},
};
use std::thread;
use std::time::Duration;
const PTY_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_FIXTURE_TOKEN: &str = "remote-a12-fixture-token-0123456789";
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
            r#"{"protocol":"bitrouter-control","protocol_version":2,"server_version":"REMOTE_A12_CAPABILITIES","server_instance_id":"47da0290-4510-4f39-ab53-a0b09ba707c2","scopes":["control:read"],"action_descriptors":{"status":{"version":1,"required_scope":"control:read","input_schema":{},"output_schema":{}},"list_models":{"version":2,"required_scope":"control:read","input_schema":{},"output_schema":{}},"route":{"version":2,"required_scope":"control:read","input_schema":{},"output_schema":{}},"requests":{"version":1,"required_scope":"control:read","input_schema":{},"output_schema":{}}}}"#,
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

impl RemoteFixture {
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"));
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

#[tokio::test]
async fn remote_context_rejects_background_and_session_mutations_before_local_start() -> Result<()>
{
    let remote = RemoteFixture::new(RemoteScenario::Healthy)?;
    let invocations = [
        vec![
            "--context",
            "fixture",
            "run",
            "stub",
            "must remain remote",
            "--direct",
            "--background",
        ],
        vec!["--context", "fixture", "agents", "review", "run-1"],
        vec!["--context", "fixture", "agents", "stop", "run-1"],
        vec!["--context", "fixture", "agents", "remove", "run-1"],
    ];

    for args in invocations {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &remote.home_path)
            .env("BITROUTER_HOME", &remote.bitrouter_home_path)
            .env("TMPDIR", &remote.temporary_path)
            .env("NO_COLOR", "1")
            .env("BITROUTER_A12_TOKEN", REMOTE_FIXTURE_TOKEN)
            .current_dir(remote._directory.path())
            .kill_on_drop(true)
            .output()
            .await?;
        assert!(!output.status.success());
        let error = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            (error.contains("local-only") || error.contains("unrecognized subcommand")),
            "remote execution was not rejected by the pre-dispatch gate: {error}"
        );
        ensure!(
            !error.contains("BITROUTER_HOME") && !error.contains("bitrouter.yaml"),
            "remote execution reached local config resolution: {error}"
        );
    }

    assert!(!remote.bitrouter_home_path.join("bitrouter.sock").exists());
    assert!(!remote.bitrouter_home_path.join("bitrouter.pid").exists());
    Ok(())
}
