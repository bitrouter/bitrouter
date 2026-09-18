//! Real foreground-host acceptance: TCP inference, both management transports,
//! startup failures and process-local evidence across a custom-host restart.
#![cfg(unix)]

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use bitrouter::actions::checks::ChecksReport;
use bitrouter::daemon::{
    self, DaemonCommand, DaemonInspection, DaemonInspectionReport, DaemonResponse,
};
use bitrouter::paths::ConfigSource;
use bitrouter::reload::RunningConfigState;
use bitrouter_sdk::extension::request_check::Decision;
use bitrouter_sdk::language_model::receipts::{
    RequestCheckStatus, RequestReceipt, RequestReceiptLookup, RequestReceiptOutcome,
    RequestReceiptUnknownReason,
};
use bitrouter_sdk::language_model::request_checks::CheckerFailureKind;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD_CONFIG: &str = "BITROUTER_TEST_EXTENSION_HOST_CONFIG";
const CHILD_MODE: &str = "BITROUTER_TEST_EXTENSION_HOST_MODE";
const TOKEN: &str = "extension-host-test-token-at-least-32-bytes";

// The parent invokes only this test in an isolated process. A normal suite run
// has no child configuration and exits immediately without global state changes.
#[tokio::test]
async fn extension_host_child() -> Result<()> {
    let Some(config) = std::env::var_os(CHILD_CONFIG) else {
        return Ok(());
    };
    let mode = std::env::var(CHILD_MODE)?;
    bitrouter::host::serve_with_extensions(&ConfigSource::File(config.into()), move |api| {
        if mode == "missing" {
            return Ok(());
        }
        api.request_check(
            "fixture",
            "rules-v1",
            Arc::new(|input| {
                let text = input
                    .content
                    .iter()
                    .filter_map(|fragment| fragment.text.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n");
                match text.as_str() {
                    "deny" => Decision::Deny {
                        reason_code: "fixture.denied".into(),
                    },
                    "invalid" => Decision::Deny {
                        reason_code: "not a valid reason code".into(),
                    },
                    "timeout" => {
                        std::thread::sleep(Duration::from_millis(1_100));
                        Decision::Allow
                    }
                    _ => Decision::Allow,
                }
            }),
        )?;
        if mode == "duplicate" {
            // Ignoring a registration error must still poison activation.
            let _ = api.request_check("fixture", "rules-v1", Arc::new(|_| Decision::Allow));
        }
        api.request_check(
            "unused",
            "unused-v1",
            Arc::new(|_| Decision::Deny {
                reason_code: "must.not.run".into(),
            }),
        )?;
        Ok(())
    })
    .await
}

struct Host {
    child: Child,
    log: PathBuf,
}

impl Host {
    fn spawn(home: &Path, mode: &str) -> Result<Self> {
        let log = home.join(format!("host-{mode}.log"));
        let output = std::fs::File::create(&log)?;
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", "extension_host_child", "--nocapture"])
            .env(CHILD_CONFIG, home.join("bitrouter.yaml"))
            .env(CHILD_MODE, mode)
            .env("BITROUTER_HOME", home)
            .env("BITROUTER_CONTROL_TOKEN", TOKEN)
            .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(Self { child, log })
    }

    fn official(home: &Path) -> Result<Self> {
        let log = home.join("official.log");
        let output = std::fs::File::create(&log)?;
        let child = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(["serve", "--config"])
            .arg(home.join("bitrouter.yaml"))
            .env("BITROUTER_HOME", home)
            .env("BITROUTER_CONTROL_TOKEN", TOKEN)
            .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(Self { child, log })
    }

    async fn exited(&mut self) -> Result<ExitStatus> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .with_context(|| format!("host did not exit: {}", self.logs()))?
    }

    fn logs(&self) -> String {
        match std::fs::read_to_string(&self.log) {
            Ok(logs) => logs,
            Err(error) => format!("cannot read host log: {error}"),
        }
    }

    async fn ready(
        &mut self,
        home: &Path,
        inference: SocketAddr,
        control: SocketAddr,
    ) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(400))
            .build()?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                ensure!(
                    self.child.try_wait()?.is_none(),
                    "host exited before readiness: {}",
                    self.logs()
                );
                let socket_ready = matches!(
                    daemon::send_command(&home.join("host.sock"), &DaemonCommand::Status).await,
                    Ok(DaemonResponse::Status { .. })
                );
                let http_ready = client
                    .get(format!("http://{inference}/health"))
                    .send()
                    .await
                    .is_ok();
                let remote_ready = client
                    .get(format!("http://{control}/control/v1/checks"))
                    .bearer_auth(TOKEN)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success());
                if socket_ready && http_ready && remote_ready {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .with_context(|| format!("host readiness timed out: {}", self.logs()))?
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Every error path reaps the child before the temporary home is removed.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temporary_home() -> Result<TempDir> {
    // Short paths avoid macOS's 104-byte Unix-socket path limit.
    Ok(tempfile::Builder::new()
        .prefix("bro-ext-")
        .tempdir_in("/tmp")?)
}

fn addresses() -> Result<(SocketAddr, SocketAddr)> {
    let inference = TcpListener::bind("127.0.0.1:0")?;
    let control = TcpListener::bind("127.0.0.1:0")?;
    Ok((inference.local_addr()?, control.local_addr()?))
}

fn write_config(
    home: &Path,
    inference: SocketAddr,
    control: SocketAddr,
    upstream: &str,
    revision: &str,
) -> Result<()> {
    std::fs::write(
        home.join("bitrouter.yaml"),
        format!(
            r#"inherit_defaults: false
registry:
  enabled: false
server:
  listen: {inference}
  control_socket: host.sock
  skip_auth: true
control:
  enabled: true
  listen: {control}
database:
  url: 'sqlite://host.db?mode=rwc'
providers:
  fixture:
    api_base: {upstream}
    api_key: fixture-test-key
    models:
      - id: model
checkers:
  fixture:
    native:
      revision: {revision}
routers:
  coding:
    selection:
      kind: model
      model: fixture:model
    checks:
      request:
        - checker: fixture
          timeout_ms: 500
"#
        ),
    )?;
    Ok(())
}

async fn inspect(home: &Path, inspection: DaemonInspection) -> Result<DaemonInspectionReport> {
    match daemon::send_command(
        &home.join("host.sock"),
        &DaemonCommand::Inspect { inspection },
    )
    .await?
    {
        DaemonResponse::Inspection { report } => Ok(report),
        other => bail!("unexpected management reply: {other:?}"),
    }
}

async fn checks(home: &Path) -> Result<ChecksReport> {
    match inspect(home, DaemonInspection::Checks).await? {
        DaemonInspectionReport::Checks(report) => Ok(report),
        other => bail!("unexpected checks reply: {other:?}"),
    }
}

async fn lookup(
    home: &Path,
    id: &str,
    incarnation: Option<String>,
) -> Result<RequestReceiptLookup> {
    match inspect(
        home,
        DaemonInspection::CheckReceipt {
            request_id: id.into(),
            incarnation,
        },
    )
    .await?
    {
        DaemonInspectionReport::CheckReceipt(report) => Ok(report),
        other => bail!("unexpected receipt reply: {other:?}"),
    }
}

async fn terminal_receipt(home: &Path, id: &str) -> Result<RequestReceipt> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RequestReceiptLookup::Found { receipt, .. } = lookup(home, id, None).await?
                && receipt.outcome.is_some()
            {
                return Ok(receipt);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("receipt remained nonterminal")?
}

fn ensure_clean(home: &Path) -> Result<()> {
    ensure!(
        !home.join("host.sock").exists(),
        "socket survived host shutdown"
    );
    ensure!(
        !home.join("host.pid").exists(),
        "PID file survived host shutdown"
    );
    for entry in std::fs::read_dir(home)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        ensure!(
            !(name.starts_with(".bitrouter-daemon-") && name.ends_with(".json")),
            "locator survived shutdown: {name}"
        );
    }
    Ok(())
}

async fn mount_upstream(upstream: &MockServer) {
    let first = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]});
    let last = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}});
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream":true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .with_priority(1)
        .mount(upstream)
        .await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"fixture-response","object":"chat.completion","model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}})))
        .with_priority(2).mount(upstream).await;
}

#[tokio::test]
async fn compiled_host_shares_inference_management_reload_and_shutdown() -> Result<()> {
    let home = temporary_home()?;
    let (inference, control) = addresses()?;
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    write_config(home.path(), inference, control, &upstream.uri(), "rules-v1")?;
    let mut host = Host::spawn(home.path(), "normal")?;
    host.ready(home.path(), inference, control).await?;
    let initial = checks(home.path()).await?;
    ensure!(
        initial.checkers.len() == 1,
        "unused registration appeared in inventory"
    );
    ensure!(
        initial.checkers[0]
            .bindings
            .iter()
            .all(|binding| binding.last_actual.is_none())
    );
    let incarnation = initial.receipt_retention.incarnation_id;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let remote = format!("http://{control}/control/v1");
    ensure!(
        client
            .get(format!("{remote}/checks"))
            .send()
            .await?
            .status()
            == reqwest::StatusCode::UNAUTHORIZED
    );

    for stream in [false, true] {
        for action in ["allow", "deny", "timeout", "invalid"] {
            let id = format!("{action}-{stream}");
            let response = client.post(format!("http://{inference}/v1/chat/completions"))
                .header("x-bitrouter-request-id", &id)
                .json(&json!({"model":"bitrouter/coding","messages":[{"role":"user","content":action}],"stream":stream}))
                .send().await?;
            ensure!(
                response.status().is_success() == (action == "allow"),
                "unexpected status for {id}: {}",
                response.status()
            );
            let body = response.text().await?;
            if stream && action == "allow" {
                ensure!(body.contains("[DONE]"), "incomplete stream: {body}");
            }
            let receipt = terminal_receipt(home.path(), &id).await?;
            ensure!(receipt.upstream_started == (action == "allow"));
            ensure!(receipt.identity.incarnation_id == incarnation);
            let expected = match action {
                "allow" => RequestCheckStatus::Allowed,
                "deny" => RequestCheckStatus::Denied,
                _ => RequestCheckStatus::Failed,
            };
            ensure!(receipt.checks[0].status == expected, "{id}: {receipt:?}");
            let failure_kind = match action {
                "timeout" => Some(CheckerFailureKind::Timeout),
                "invalid" => Some(CheckerFailureKind::InvalidResponse),
                _ => None,
            };
            ensure!(
                receipt.checks[0].failure_kind == failure_kind,
                "{id}: {receipt:?}"
            );
            if action == "allow" {
                ensure!(receipt.outcome == Some(RequestReceiptOutcome::Completed));
            }
            let remote_receipt: RequestReceiptLookup = client
                .get(format!("{remote}/checks/receipts/{id}"))
                .bearer_auth(TOKEN)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(
                matches!(remote_receipt, RequestReceiptLookup::Found { receipt: remote, .. } if remote == receipt),
                "local and remote receipts disagree"
            );
        }
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing capture")?
            .len()
            == 2,
        "rejected check reached provider"
    );
    let local = checks(home.path()).await?;
    let remote_checks: ChecksReport = client
        .get(format!("{remote}/checks"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(remote_checks.receipt_retention == local.receipt_retention);
    ensure!(serde_json::to_value(remote_checks.checkers)? == serde_json::to_value(local.checkers)?);

    write_config(home.path(), inference, control, &upstream.uri(), "rules-v2")?;
    let reload = daemon::send_command(
        &home.path().join("host.sock"),
        &DaemonCommand::Reload { env: vec![] },
    )
    .await?;
    ensure!(
        !matches!(reload, DaemonResponse::Ok),
        "checker edit reloaded unexpectedly"
    );
    let changed = checks(home.path()).await?;
    ensure!(
        changed
            .config_state
            .context("missing config evidence")?
            .running
            == RunningConfigState::RestartRequired
    );
    ensure!(changed.checkers[0].revision == "rules-v1");
    write_config(home.path(), inference, control, &upstream.uri(), "rules-v1")?;
    ensure!(matches!(
        daemon::send_command(&home.path().join("host.sock"), &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    ensure!(
        host.exited().await?.success(),
        "stop failed: {}",
        host.logs()
    );
    ensure_clean(home.path())?;

    let mut restarted = Host::spawn(home.path(), "normal")?;
    restarted.ready(home.path(), inference, control).await?;
    ensure!(checks(home.path()).await?.receipt_retention.incarnation_id != incarnation);
    ensure!(matches!(
        lookup(home.path(), "allow-false", Some(incarnation)).await?,
        RequestReceiptLookup::Unknown {
            reason: RequestReceiptUnknownReason::IncarnationMismatch,
            ..
        }
    ));
    ensure!(matches!(
        lookup(home.path(), "allow-false", None).await?,
        RequestReceiptLookup::Unknown {
            reason: RequestReceiptUnknownReason::NotRetained,
            ..
        }
    ));
    let status = Command::new("kill")
        .args(["-TERM", &restarted.child.id().to_string()])
        .status()?;
    ensure!(status.success(), "failed to deliver SIGTERM");
    ensure!(
        restarted.exited().await?.success(),
        "SIGTERM failed: {}",
        restarted.logs()
    );
    ensure_clean(home.path())?;
    Ok(())
}

#[tokio::test]
async fn compiled_host_rejects_registration_before_database_and_cleans_bind_failures() -> Result<()>
{
    for mode in [
        "missing",
        "duplicate",
        "revision",
        "inference_collision",
        "control_collision",
    ] {
        let home = temporary_home()?;
        let (mut inference, mut control) = addresses()?;
        let occupied = TcpListener::bind("127.0.0.1:0")?;
        if mode == "inference_collision" {
            inference = occupied.local_addr()?;
        }
        if mode == "control_collision" {
            control = occupied.local_addr()?;
        }
        let revision = if mode == "revision" {
            "rules-v2"
        } else {
            "rules-v1"
        };
        write_config(
            home.path(),
            inference,
            control,
            "http://127.0.0.1:1",
            revision,
        )?;
        let mut host = Host::spawn(home.path(), mode)?;
        ensure!(
            !host.exited().await?.success(),
            "{mode} startup unexpectedly succeeded"
        );
        ensure_clean(home.path())?;
        ensure!(
            !host.logs().contains("— serving on"),
            "failed startup advertised readiness: {}",
            host.logs()
        );
        if matches!(mode, "missing" | "duplicate" | "revision") {
            ensure!(
                !home.path().join("host.db").exists(),
                "{mode} reached database assembly: {}",
                host.logs()
            );
            let expected = match mode {
                "missing" => "not registered",
                "duplicate" => "already registered",
                _ => "revision does not match",
            };
            ensure!(
                host.logs().contains(expected),
                "wrong {mode} failure: {}",
                host.logs()
            );
        } else {
            ensure!(
                home.path().join("host.db").exists(),
                "collision failed before assembly: {}",
                host.logs()
            );
            let expected = if mode == "inference_collision" {
                "bind inference listener"
            } else {
                "bind remote control listener"
            };
            ensure!(
                host.logs().contains(expected),
                "wrong {mode} failure: {}",
                host.logs()
            );
        }
        if mode == "inference_collision" {
            let _released = TcpListener::bind(control)
                .context("remote listener survived inference bind failure")?;
        }
        if mode == "control_collision" {
            let _released = TcpListener::bind(inference)
                .context("inference listener survived control bind failure")?;
        }
    }
    Ok(())
}

async fn cli(home: &Path, command: &str) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
            .kill_on_drop(true)
            .args([command, "--config"])
            .arg(home.join("bitrouter.yaml"))
            .env("BITROUTER_HOME", home)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("management CLI timed out")??)
}

#[tokio::test]
async fn official_cli_uses_shared_host_without_extension_registrations() -> Result<()> {
    let home = temporary_home()?;
    let (inference, control) = addresses()?;
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    write_config(home.path(), inference, control, &upstream.uri(), "rules-v1")?;
    let config_path = home.path().join("bitrouter.yaml");
    let config = std::fs::read_to_string(&config_path)?;
    let (without_checks, _) = config
        .split_once("checkers:\n")
        .context("fixture config has no checkers")?;
    std::fs::write(&config_path, without_checks)?;
    let mut host = Host::official(home.path())?;
    host.ready(home.path(), inference, control).await?;
    let output = cli(home.path(), "checks").await?;
    ensure!(
        output.status.success(),
        "checks CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: ChecksReport = serde_json::from_slice(&output.stdout)?;
    ensure!(report.checkers.is_empty());
    ensure!(report.receipt_retention == checks(home.path()).await?.receipt_retention);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?
        .post(format!("http://{inference}/v1/chat/completions"))
        .json(&json!({"model":"fixture:model","messages":[{"role":"user","content":"allow"}]}))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "official host inference failed: {}",
        response.text().await?
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing capture")?
            .len()
            == 1
    );
    let output = cli(home.path(), "stop").await?;
    ensure!(
        output.status.success(),
        "stop CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        host.exited().await?.success(),
        "official host failed: {}",
        host.logs()
    );
    ensure_clean(home.path())?;
    Ok(())
}
