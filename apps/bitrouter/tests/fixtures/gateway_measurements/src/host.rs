use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(super) struct Host {
    child: Child,
    socket: PathBuf,
    pub(super) endpoint: String,
    pub(super) startup_ms: f64,
    // Drop the child before its private files.
    directory: tempfile::TempDir,
}

impl Host {
    pub(super) async fn start(binary: &Path, upstream: &str) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("bro-measure-")
            .tempdir_in("/tmp")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        for name in ["home", "runtime", "tmp"] {
            fs::create_dir(directory.path().join(name))?;
        }
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let listen = reservation.local_addr()?;
        let config = directory.path().join("private.yaml");
        let socket = directory.path().join("control.sock");
        fs::write(
            &config,
            format!(
                r#"inherit_defaults: false
registry:
  enabled: false
server:
  listen: "{listen}"
  control_socket: control.sock
  skip_auth: true
database:
  url: "sqlite://measure.db?mode=rwc"
agent_api:
  enabled: false
policy:
  mode: frozen
providers:
  fixture:
    api_base: "{upstream}"
    api_key: fixture-only-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: probe
        pricing:
          input_micro_usd_per_token: 1
          output_micro_usd_per_token: 2
"#
            ),
        )?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
        let log = File::create(directory.path().join("daemon.log"))?;
        let started = Instant::now();
        drop(reservation);
        let child = Command::new(binary)
            .args(["--json", "serve", "--config"])
            .arg(&config)
            .arg("--managed-child")
            .current_dir(directory.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory.path().join("home"))
            .env("USERPROFILE", directory.path().join("home"))
            .env("XDG_CONFIG_HOME", directory.path().join("home"))
            .env("BITROUTER_HOME", directory.path().join("runtime"))
            .env("TMPDIR", directory.path().join("tmp"))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let mut host = Self {
            child,
            socket,
            endpoint: format!("http://{listen}"),
            startup_ms: 0.0,
            directory,
        };
        // The process-test harness uses this ownership record for interruption
        // checks and fallback cleanup. Never discover or stop a resident daemon.
        eprintln!("owned_gateway {} {}", host.child.id(), config.display());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let deadline = started + Duration::from_secs(30);
        loop {
            ensure!(
                host.child.try_wait()?.is_none(),
                "gateway exited: {}",
                host.log()
            );
            if let Ok(status) = host.control("status").await {
                ensure!(status["resp"] == "status" && status["pid"] == host.child.id());
                ensure!(status["listen"] == listen.to_string());
                if let Ok(response) = client.get(format!("{}/health", host.endpoint)).send().await
                    && response.status().is_success()
                {
                    bounded_body(response, 16 * 1024).await?;
                    host.startup_ms = started.elapsed().as_secs_f64() * 1000.0;
                    return Ok(host);
                }
            }
            ensure!(
                Instant::now() < deadline,
                "gateway readiness deadline: {}",
                host.log()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.directory.path().join("daemon.log"))
            .unwrap_or_else(|error| format!("log unavailable: {error}"))
    }

    async fn control(&self, command: &str) -> Result<Value> {
        // Existing newline-delimited private control protocol, shared by baseline
        // and candidate: apps/bitrouter/src/daemon.rs::send_command.
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            stream
                .write_all(format!("{{\"cmd\":\"{command}\"}}\n").as_bytes())
                .await?;
            stream.flush().await?;
            let mut reader = BufReader::new(stream.take(1024 * 1024));
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            ensure!(line.ends_with('\n'), "incomplete control frame");
            Ok(serde_json::from_str(&line)?)
        })
        .await
        .context("control timeout")?
    }

    pub(super) async fn rss_kib(&mut self) -> Result<u64> {
        ensure!(
            self.child.try_wait()?.is_none(),
            "gateway exited before RSS sample"
        );
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::process::Command::new("/bin/ps")
                .args(["-o", "rss=", "-p"])
                .arg(self.child.id().to_string())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(output.status.success(), "ps failed");
        let rss = std::str::from_utf8(&output.stdout)?.trim().parse()?;
        ensure!(rss > 0, "empty RSS sample");
        Ok(rss)
    }

    pub(super) async fn stop(&mut self) -> Result<()> {
        let status = self.control("status").await?;
        ensure!(
            status["pid"] == self.child.id(),
            "private control owner changed"
        );
        ensure!(self.control("stop").await?["resp"] == "ok");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(status.success(), "gateway failed shutdown: {}", self.log());
                ensure!(!self.socket.exists(), "owned socket was not removed");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "gateway shutdown deadline");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

pub(super) async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= limit - body.len(),
            "response exceeds {limit} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Drop for Host {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Err(error) = self.child.kill() {
            eprintln!("owned gateway cleanup kill failed: {error}");
            return;
        }
        if let Err(error) = self.child.wait() {
            eprintln!("owned gateway reap failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[tokio::test]
    async fn body_limit_rejects_oversized_responses() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("12345"))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        assert!(
            bounded_body(client.get(server.uri()).send().await?, 4)
                .await
                .is_err()
        );
        assert_eq!(
            bounded_body(client.get(server.uri()).send().await?, 5).await?,
            b"12345"
        );
        Ok(())
    }
}
