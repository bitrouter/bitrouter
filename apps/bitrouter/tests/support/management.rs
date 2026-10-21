//! Shared headless process fixture; no terminal, UI reducer or user credentials.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};

#[path = "../../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
pub(super) mod example_binary;

pub struct Fixture {
    pub directory: TempDir,
    pub workspace: PathBuf,
    pub config: PathBuf,
    pub capture: PathBuf,
    pub control: PathBuf,
    pub socket: PathBuf,
    pub daemon: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        use std::io::{BufRead, Write};
        let Some(child) = self.daemon.as_mut() else {
            return;
        };
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            if stream.write_all(b"{\"cmd\":\"stop\"}\n").is_ok() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(stream).read_line(&mut line);
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(7);
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.start_kill();
    }
}

impl Fixture {
    pub async fn new(scenario: Option<&str>, provider: Option<&str>) -> Result<Self> {
        let directory = tempfile::tempdir_in("/tmp")?;
        let workspace = directory.path().join("project");
        std::fs::create_dir(&workspace)?;
        let config = directory.path().join("bitrouter.yaml");
        let capture = directory.path().join("requests.ndjson");
        let control = directory.path().join("agent-control.sock");
        let socket = directory.path().join("bitrouter.sock");
        // Routed controller preflight consumes the configured URL. Reserve a
        // concrete ephemeral port for that journey, rather than advertising :0.
        let reservation = if scenario == Some("control-settings") {
            Some(std::net::TcpListener::bind("127.0.0.1:0")?)
        } else {
            None
        };
        let listen = reservation
            .as_ref()
            .map(|listener| listener.local_addr())
            .transpose()?
            .map(|address| address.to_string())
            .unwrap_or_else(|| "127.0.0.1:0".into());
        let mut value = json!({"inherit_defaults":false,"registry":{"enabled":false},
            "server":{"listen":listen,"skip_auth":true},
            "database":{"url":format!("sqlite://{}?mode=rwc",directory.path().join("state.db").display())},
            "agent_api":{"workspaces":[workspace]},
            "acp_recording":{"enabled":true},
        });
        if let Some(provider) = provider {
            value["providers"] = json!({"mock":{"api_base":provider,"api_key":"fixture",
                "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"},{"id":"alt"}]}});
            value["models"] = json!({
                "coding":{"endpoints":[{"provider":"mock","service_id":"model"}]},
                "candidate":{"endpoints":[{"provider":"mock","service_id":"alt"}]}
            });
        }
        if let Some(scenario) = scenario {
            static EXECUTABLE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
            let executable = EXECUTABLE
                .get_or_init(|| example_binary::build("bitrouter", "pty_acp_fixture"))
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            value["agents"] = json!({"stub":{"name":"stub","transport":{"type":"stdio","command":executable,
                "env":{"BITROUTER_PTY_SCENARIO":scenario,"BITROUTER_PTY_CAPTURE":capture,"BITROUTER_PTY_CONTROL":control}}}});
            if scenario == "control-settings" {
                let (package, version) = bitrouter::harness::by_id("claude-acp")
                    .and_then(|harness| harness.maintained_adapter_identity())
                    .context("fixture routing identity missing")?;
                // Exercise the production route bridge with a Cargo peer. This
                // marker does not run or qualify the real maintained adapter.
                value["agents"]["stub"]["transport"]["args"] =
                    json!([format!("{package}@{version}")]);
            }
        }
        std::fs::write(&config, serde_json::to_vec_pretty(&value)?)?;
        let mut fixture = Self {
            directory,
            workspace,
            config,
            capture,
            control,
            socket,
            daemon: None,
        };
        drop(reservation);
        fixture.start().await?;
        Ok(fixture)
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.directory.path())
            .env("BITROUTER_HOME", self.directory.path())
            .env("XDG_DATA_HOME", self.directory.path().join("data"))
            .env("XDG_CACHE_HOME", self.directory.path().join("cache"))
            .env("NO_COLOR", "1")
            .current_dir(self.directory.path())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    pub async fn start(&mut self) -> Result<()> {
        ensure!(self.daemon.is_none(), "fixture daemon already owned");
        self.daemon = Some(
            self.command()
                .args(["serve", "--config"])
                .arg(&self.config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(daemon) = self.daemon.as_mut()
                    && let Some(status) = daemon.try_wait()?
                {
                    anyhow::bail!("fixture daemon exited {status}");
                }
                if bitrouter::daemon::probe_status(&self.socket)
                    .await?
                    .is_some()
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        Ok(())
    }

    pub async fn stop(&mut self, abrupt: bool) -> Result<()> {
        let Some(mut child) = self.daemon.take() else {
            return Ok(());
        };
        if abrupt {
            child.start_kill()?;
        } else {
            bitrouter::daemon::send_command(&self.socket, &bitrouter::daemon::DaemonCommand::Stop)
                .await?;
        }
        tokio::time::timeout(Duration::from_secs(15), child.wait()).await??;
        Ok(())
    }

    pub async fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(
            tokio::time::timeout(Duration::from_secs(65), self.command().args(args).output())
                .await??,
        )
    }

    pub async fn run(&self, args: &[&str]) -> Result<Value> {
        let output = self.output(args).await?;
        ensure!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            !output.stdout.contains(&0x1b),
            "CLI emitted terminal controls"
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    pub async fn rejected(&self, args: &[&str]) -> Result<Value> {
        let output = self.output(args).await?;
        ensure!(!output.status.success(), "unexpectedly accepted {args:?}");
        Ok(serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| json!({"stderr":String::from_utf8_lossy(&output.stderr)})))
    }

    pub fn prompt(&self, name: &str, text: &str) -> Result<PathBuf> {
        let path = self.directory.path().join(name);
        std::fs::write(&path, text)?;
        Ok(path)
    }

    pub fn captured(&self) -> Result<Vec<Value>> {
        match std::fs::read(&self.capture) {
            // A live peer can be midway through one JSON record. Only newline
            // commits a fixture record; malformed completed records still fail.
            Ok(bytes) => {
                let end = bytes
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index + 1);
                bytes[..end]
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(|line| Ok(serde_json::from_slice(line)?))
                    .collect()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn control(&self, text: &str) -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(&self.control).await?;
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(format!("{text}\n").as_bytes())
            .await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
        ensure!(line.trim() == "ok", "fixture control failed");
        Ok(())
    }

    pub async fn wait(&self, args: &[&str], ready: impl Fn(&Value) -> bool) -> Result<Value> {
        let mut last = Value::Null;
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let value = self.run(args).await?;
                if ready(&value) {
                    return Ok(value);
                }
                last = value;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        result.with_context(|| format!("waiting for {args:?}; last: {last}"))?
    }

    pub async fn background(&self, prompt: &str, options: &[&str]) -> Result<String> {
        let mut args = vec![
            "run",
            "stub",
            prompt,
            "--background",
            "--cwd",
            path(&self.workspace)?,
        ];
        args.extend_from_slice(options);
        Ok(self.run(&args).await?["agent_run_id"]
            .as_str()
            .context("run id missing")?
            .into())
    }
}

pub fn path(path: &Path) -> Result<&str> {
    path.to_str().context("fixture path is not UTF-8")
}
