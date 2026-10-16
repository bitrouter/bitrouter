//! Acceptance against two separately built, immutable executables from one source revision.
//!
//! Set BITROUTER_TEST_FULL_BINARY and BITROUTER_TEST_GATEWAY_BINARY to absolute
//! paths before explicitly selecting this ignored test. The latter must have
//! no application features. This test never builds or substitutes an artifact.
//! The historical-data transition test additionally requires
//! BITROUTER_TEST_HARNESS_BINARY built with --no-default-features --features harness.

#![cfg(unix)]

#[path = "artifact_profiles/persistence.rs"]
mod persistence;

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse};
use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use bitrouter_ai::control::status::StatusReport;
use serde::de::DeserializeOwned;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);

fn artifact_path(variable: &str) -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os(variable).with_context(|| {
        format!("{variable} must name a separately built executable when this test is selected")
    })?);
    ensure!(path.is_absolute(), "{variable} must be absolute: {path:?}");
    let metadata = fs::metadata(&path).with_context(|| format!("reading {variable}: {path:?}"))?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "{variable} is not an executable file: {path:?}"
    );
    fs::canonicalize(&path).with_context(|| format!("resolving {variable}: {path:?}"))
}

fn tail(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(16 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(16 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Process {
    child: Child,
    resident: bool,
    label: String,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Process {
    fn diagnostic(&self) -> String {
        format!(
            "{} (pid {})\nstdout:\n{}\nstderr:\n{}",
            self.label,
            self.child.id(),
            tail(&self.stdout).unwrap_or_else(|error| error.to_string()),
            tail(&self.stderr).unwrap_or_else(|error| error.to_string()),
        )
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(POLL);
        }
        eprintln!("could not reap owned child: {}", self.diagnostic());
    }
}

struct CliOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    diagnostic: String,
}

impl CliOutput {
    fn json<T: DeserializeOwned>(&self) -> Result<T> {
        ensure!(self.status.success(), "{}", self.diagnostic);
        serde_json::from_slice(&self.stdout)
            .with_context(|| format!("invalid CLI JSON: {}", self.diagnostic))
    }

    fn rejected(&self, reason: &str) -> Result<()> {
        ensure!(
            !self.status.success(),
            "unexpected success: {}",
            self.diagnostic
        );
        let report: serde_json::Value = serde_json::from_slice(&self.stdout)
            .with_context(|| format!("missing structured rejection: {}", self.diagnostic))?;
        ensure!(
            report["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(reason)),
            "expected rejection containing {reason:?}: {}",
            self.diagnostic
        );
        Ok(())
    }
}

struct Fixture {
    config: PathBuf,
    socket: PathBuf,
    sockets: BTreeSet<PathBuf>,
    runtime: tokio::runtime::Runtime,
    processes: Vec<Process>,
    observed_pids: BTreeSet<u32>,
    unsettled_launch: bool,
    cleaned: bool,
    // Drop child handles before removing their files, including on unwind.
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        // A short private path also fits macOS's Unix-socket path limit. Never
        // derive a lifecycle target from the invoking user's configuration.
        let directory = tempfile::Builder::new()
            .prefix("bro-profiles-")
            .tempdir_in("/tmp")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        for name in ["home", "runtime", "tmp"] {
            fs::create_dir(directory.path().join(name))?;
        }
        let config = directory.path().join("private.yaml");
        let socket = directory.path().join("control.sock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            directory,
            config,
            sockets: BTreeSet::from([socket.clone()]),
            socket,
            runtime,
            processes: Vec::new(),
            observed_pids: BTreeSet::new(),
            unsettled_launch: false,
            cleaned: false,
        })
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(self.directory.path())
            // Includes removal of all OTEL_*, credentials, contexts, feature
            // activation variables and harness discovery overrides.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.directory.path().join("home"))
            .env("USERPROFILE", self.directory.path().join("home"))
            .env("XDG_CONFIG_HOME", self.directory.path().join("home"))
            .env("BITROUTER_HOME", self.directory.path().join("runtime"))
            .env("TMPDIR", self.directory.path().join("tmp"))
            .stdin(Stdio::null());
        command
    }

    fn selected_command(&self, binary: &Path, action: &str) -> Command {
        let mut command = self.command(binary);
        command
            .args(["--json", action, "--config"])
            .arg(&self.config);
        command
    }

    fn spawn(&mut self, mut command: Command, resident: bool) -> Result<usize> {
        let index = self.processes.len();
        let stdout = self
            .directory
            .path()
            .join(format!("process-{index}.stdout"));
        let stderr = self
            .directory
            .path()
            .join(format!("process-{index}.stderr"));
        let label = format!("{:?} {:?}", command.get_program(), command.get_args());
        let child = command
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .spawn()
            .with_context(|| format!("spawning {label}"))?;
        self.processes.push(Process {
            child,
            resident,
            label,
            stdout,
            stderr,
        });
        Ok(index)
    }

    fn reap(&mut self) -> Result<()> {
        // Reap resident children while the restart CLI is running. Otherwise
        // its kill(0) check can see our old child as a zombie and time out.
        for process in &mut self.processes {
            process.child.try_wait()?;
        }
        Ok(())
    }

    fn run(&mut self, command: Command) -> Result<CliOutput> {
        let may_launch = command
            .get_args()
            .any(|arg| arg == "restart" || arg == "__daemon-handoff");
        let index = self.spawn(command, false)?;
        self.unsettled_launch |= may_launch;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            self.reap()?;
            let process = &mut self.processes[index];
            if let Some(status) = process.child.try_wait()? {
                let output = CliOutput {
                    status,
                    stdout: fs::read(&process.stdout)?,
                    diagnostic: format!("exit {status}: {}", process.diagnostic()),
                };
                if may_launch {
                    let _ = self.control(&DaemonCommand::Status);
                }
                return Ok(output);
            }
            ensure!(
                Instant::now() < deadline,
                "CLI deadline exceeded: {}",
                process.diagnostic()
            );
            std::thread::sleep(POLL);
        }
    }

    fn control(&mut self, command: &DaemonCommand) -> Result<DaemonResponse> {
        self.control_at(&self.socket.clone(), command)
    }

    fn control_at(&mut self, socket: &Path, command: &DaemonCommand) -> Result<DaemonResponse> {
        let reply = self.runtime.block_on(async {
            tokio::time::timeout(CONTROL_TIMEOUT, daemon::send_command(socket, command))
                .await
                .context("private control request timed out")?
        })?;
        if let DaemonResponse::Status { pid, .. } = &reply {
            self.observed_pids.insert(*pid);
            self.unsettled_launch = false;
        }
        Ok(reply)
    }

    fn start(&mut self, binary: &Path) -> Result<u32> {
        let mut command = self.selected_command(binary, "serve");
        command.arg("--managed-child");
        let index = self.spawn(command, true)?;
        let pid = self.processes[index].child.id();
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            ensure!(
                self.processes[index].child.try_wait()?.is_none(),
                "resident exited before readiness: {}",
                self.processes[index].diagnostic()
            );
            let reply = self.control(&DaemonCommand::Status);
            if let Ok(DaemonResponse::Status { pid: reported, .. }) = &reply {
                ensure!(
                    *reported == pid,
                    "private socket answered with unexpected PID: {reply:?}"
                );
                return Ok(pid);
            }
            ensure!(Instant::now() < deadline, "resident not ready: {reply:?}");
            std::thread::sleep(POLL);
        }
    }

    fn configure_and_start_full(&mut self, binary: &Path) -> Result<(String, u32)> {
        // The host currently reports its configured address, including :0.
        // Reserve an OS-assigned port and retry only a proven bind collision;
        // readiness is always observed, never inferred from elapsed time.
        for attempt in 0..5 {
            let reservation = TcpListener::bind("127.0.0.1:0")?;
            let listen = reservation.local_addr()?;
            let raw = format!(
                "inherit_defaults: false\nregistry:\n  enabled: false\nserver:\n  listen: '{listen}'\n  control_socket: control.sock\n  skip_auth: true\ndatabase:\n  url: 'sqlite://profiles.db?mode=rwc'\nagent_api:\n  enabled: false\n"
            );
            fs::write(&self.config, &raw)?;
            fs::set_permissions(&self.config, fs::Permissions::from_mode(0o600))?;
            let index = self.processes.len();
            drop(reservation);
            match self.start(binary) {
                Ok(pid) => return Ok((raw, pid)),
                Err(error) => {
                    let collision = if let Some(process) = self.processes.get_mut(index) {
                        process.child.try_wait()?.is_some()
                            && process.diagnostic().contains("Address already in use")
                    } else {
                        false
                    };
                    if !collision || attempt == 4 {
                        return Err(error);
                    }
                }
            }
        }
        bail!("exhausted loopback bind attempts")
    }

    fn status(
        &mut self,
        binary: &Path,
        installed: &ArtifactIdentity,
        resident: &ArtifactIdentity,
        pid: u32,
    ) -> Result<StatusReport> {
        let report: StatusReport = self.run(self.selected_command(binary, "status"))?.json()?;
        ensure!(
            report.running && report.pid == Some(pid),
            "wrong resident: {report:?}"
        );
        ensure!(
            report.installed_artifact.as_ref() == Some(installed),
            "wrong installed identity: {report:?}"
        );
        ensure!(
            report.daemon_artifact.as_ref() == Some(resident),
            "wrong daemon identity: {report:?}"
        );
        ensure!(
            report.installed_version.as_ref() == Some(&installed.version),
            "{report:?}"
        );
        ensure!(
            report.daemon_version.as_ref() == Some(&resident.version),
            "{report:?}"
        );
        ensure!(report.handoff_protocol == Some(2), "{report:?}");
        ensure!(
            report.handoff_build_id.as_ref() == Some(&resident.handoff_build_id),
            "{report:?}"
        );
        ensure!(report.daemon_cli_owned == Some(true), "{report:?}");
        let compatibility = if installed == resident {
            "compatible"
        } else {
            "profile_mismatch"
        };
        ensure!(
            report.compatibility.as_deref() == Some(compatibility),
            "{report:?}"
        );
        ensure!(
            report.socket.as_deref() == self.socket.to_str(),
            "CLI selected an unexpected socket: {report:?}"
        );
        self.observed_pids.insert(pid);
        Ok(report)
    }

    fn hello(&mut self, artifact: &ArtifactIdentity) -> Result<String> {
        let reply = self.control(&DaemonCommand::HandoffHello)?;
        let DaemonResponse::HandoffHello {
            protocol: 2,
            version,
            build_id,
            instance_id,
            cli_owned: true,
            artifact: Some(reported),
        } = &reply
        else {
            bail!("missing protocol-2 artifact/ownership evidence: {reply:?}");
        };
        ensure!(
            reported == artifact
                && version == &artifact.version
                && build_id == &artifact.handoff_build_id
                && !instance_id.is_empty(),
            "incorrect handshake identity: {reply:?}"
        );
        Ok(instance_id.clone())
    }

    fn health(&self, report: &StatusReport) -> Result<()> {
        let listen = report
            .listen
            .as_deref()
            .context("status omitted HTTP address")?;
        self.runtime.block_on(async {
            let response = reqwest::Client::builder()
                .no_proxy()
                .timeout(CONTROL_TIMEOUT)
                .build()?
                .get(format!("http://{listen}/health"))
                .send()
                .await?;
            ensure!(
                response.status().is_success(),
                "HTTP health failed: {report:?}"
            );
            // Fully consume the response before testing idleness.
            response.bytes().await?;
            Ok(())
        })
    }

    fn stop_resident(&mut self, pid: u32) -> Result<()> {
        ensure!(matches!(
            self.control(&DaemonCommand::Stop)?,
            DaemonResponse::Ok
        ));
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            self.reap()?;
            if !daemon::process_is_alive(pid) && !self.socket.exists() {
                for process in &mut self.processes {
                    if process.child.id() == pid {
                        ensure!(
                            process
                                .child
                                .try_wait()?
                                .is_some_and(|status| status.success()),
                            "resident shutdown failed: {}",
                            process.diagnostic()
                        );
                    }
                }
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "resident pid {pid} failed to stop"
            );
            std::thread::sleep(POLL);
        }
    }

    fn diagnostics(&self) -> String {
        let mut lines: Vec<_> = self.processes.iter().map(Process::diagnostic).collect();
        let log = self.directory.path().join("bitrouter.log");
        if log.exists() {
            lines.push(format!(
                "detached daemon log:\n{}",
                tail(&log).unwrap_or_else(|error| error.to_string())
            ));
        }
        lines.join("\n\n")
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        // Stop launchers first so they cannot create another daemon after the
        // cleanup probe. Only Child handles belonging to this fixture are killed.
        for process in &mut self.processes {
            if !process.resident && process.child.try_wait()?.is_none() {
                process.child.kill()?;
            }
        }
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            self.reap()?;
            // Discover either resident even when a transition failed before its
            // PID was saved. Both endpoints belong to this private 0700 directory.
            for socket in self.sockets.clone() {
                if matches!(
                    self.control_at(&socket, &DaemonCommand::Status),
                    Ok(DaemonResponse::Status { .. })
                ) {
                    let _ = self.control_at(&socket, &DaemonCommand::Stop);
                }
                // A private pidfile is liveness evidence, never kill authority.
                if let Ok(raw) = fs::read_to_string(socket.with_extension("pid"))
                    && let Ok(pid) = raw.trim().parse::<u32>()
                {
                    self.observed_pids.insert(pid);
                }
            }
            let children_done = self.processes.iter_mut().try_fold(true, |done, process| {
                Ok::<_, std::io::Error>(process.child.try_wait()?.is_some() && done)
            })?;
            if children_done && self.sockets.iter().all(|socket| !socket.exists())
                && self.observed_pids.iter().all(|pid| !daemon::process_is_alive(*pid))
                // If a launcher died before publishing a PID/socket, retain
                // the private directory and keep probing for the whole bound.
                && (!self.unsettled_launch || Instant::now() >= deadline)
            {
                self.cleaned = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                for process in &mut self.processes {
                    if process.child.try_wait()?.is_none() {
                        let _ = process.child.kill();
                    }
                }
                bail!(
                    "private daemon cleanup deadline exceeded; socket: {}",
                    self.socket.display()
                );
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Synchronous test/runtime ownership makes this run during unwinding,
        // too; it does not depend on a Tokio task surviving the test future.
        if let Err(error) = self.cleanup() {
            eprintln!(
                "artifact profile cleanup failed: {error:#}\n{}",
                self.diagnostics()
            );
        }
    }
}

fn exercise(fixture: &mut Fixture, full_binary: &Path, gateway_binary: &Path) -> Result<()> {
    let mut command = fixture.command(full_binary);
    command.args(["--json", "build-info"]);
    let full: ArtifactIdentity = fixture.run(command)?.json()?;
    let mut command = fixture.command(gateway_binary);
    command.args(["--json", "build-info"]);
    let gateway: ArtifactIdentity = fixture.run(command)?.json()?;
    ensure!(
        full.profile == DistributionProfile::Full,
        "full artifact: {full:?}"
    );
    ensure!(
        gateway.profile == DistributionProfile::Gateway,
        "gateway artifact: {gateway:?}"
    );
    ensure!(
        gateway.features.is_empty(),
        "gateway must have no application features: {gateway:?}"
    );
    ensure!(
        full.features.iter().any(|feature| feature == "core")
            && full.features.iter().any(|feature| feature == "harness"),
        "full native blocker requires core+harness: {full:?}"
    );
    for artifact in [&full, &gateway] {
        ensure!(
            !artifact.version.is_empty()
                && !artifact.target.is_empty()
                && !artifact.handoff_build_id.is_empty(),
            "incomplete identity: {artifact:?}"
        );
        ensure!(
            artifact
                .source_revision
                .as_ref()
                .is_some_and(|revision| !revision.is_empty()),
            "acceptance artifacts must identify their source revision: {artifact:?}"
        );
        ensure!(
            artifact.features.windows(2).all(|pair| pair[0] < pair[1]),
            "unsorted/duplicate effective features: {artifact:?}"
        );
        ensure!(
            !artifact
                .features
                .iter()
                .any(|feature| feature == "default" || feature == "otel"),
            "selector/alias leaked into effective features: {artifact:?}"
        );
    }
    ensure!(
        full.version == gateway.version
            && full.source_revision == gateway.source_revision
            && full.target == gateway.target,
        "artifacts must share version/source/target: {full:?}; {gateway:?}"
    );
    ensure!(
        full.features != gateway.features && full.handoff_build_id != gateway.handoff_build_id,
        "distinct capabilities must have distinct build fingerprints: {full:?}; {gateway:?}"
    );

    // Preserve this exact private configuration through both residents and
    // the explicit profile replacement.
    let (raw, full_pid) = fixture.configure_and_start_full(full_binary)?;
    let full_instance = fixture.hello(&full)?;
    let report = fixture.status(gateway_binary, &gateway, &full, full_pid)?;
    fixture.health(&report)?;
    ensure!(
        report
            .handoff_activity
            .as_ref()
            .is_some_and(|activity| activity.operations > 0),
        "full must retain its native lifetime admission: {report:?}"
    );

    let mut command = fixture.selected_command(gateway_binary, "__daemon-handoff");
    command.arg("--socket").arg(&fixture.socket);
    fixture.run(command)?.rejected("distribution differs")?;
    fixture.status(gateway_binary, &gateway, &full, full_pid)?;
    ensure!(
        fixture.hello(&full)? == full_instance,
        "automatic handoff replaced full"
    );

    fixture
        .run(fixture.selected_command(gateway_binary, "restart"))?
        .rejected("daemon operations are in progress")?;
    let report = fixture.status(gateway_binary, &gateway, &full, full_pid)?;
    fixture.health(&report)?;
    ensure!(
        fixture.hello(&full)? == full_instance,
        "busy restart replaced full"
    );

    let trace = fixture.directory.path().join("must-not-create.jsonl");
    let mut command = fixture.selected_command(gateway_binary, "restart");
    command.env("BITROUTER_WORKFLOW_TRACE_JSONL", &trace);
    let rejected = fixture.run(command)?;
    rejected.rejected("core feature")?;
    rejected.rejected("BITROUTER_WORKFLOW_TRACE_JSONL")?;
    fixture.status(gateway_binary, &gateway, &full, full_pid)?;
    ensure!(
        !trace.exists(),
        "unsupported trace activation created a file"
    );
    ensure!(
        fixture.hello(&full)? == full_instance,
        "feature preflight stopped full"
    );

    fixture.stop_resident(full_pid)?;
    let gateway_pid = fixture.start(gateway_binary)?;
    let gateway_instance = fixture.hello(&gateway)?;
    let report = fixture.status(full_binary, &full, &gateway, gateway_pid)?;
    fixture.health(&report)?;
    ensure!(
        report.handoff_activity.as_ref().is_some_and(|activity| {
            activity.operations == 0
                && activity.supervised_runs == Some(0)
                && activity.route_leases == 0
                && activity.acp_recordings == Some(0)
                && !activity.drain_in_progress
        }),
        "gateway is not provably idle: {report:?}"
    );

    let mut command = fixture.selected_command(full_binary, "__daemon-handoff");
    command.arg("--socket").arg(&fixture.socket);
    fixture.run(command)?.rejected("distribution differs")?;
    fixture.status(full_binary, &full, &gateway, gateway_pid)?;
    ensure!(
        fixture.hello(&gateway)? == gateway_instance,
        "automatic handoff replaced gateway"
    );

    // Explicit restart also applies a saved endpoint change. A live occupant
    // of the candidate endpoint must be rejected before stopping the resident.
    let old_socket = fixture.socket.clone();
    let replacement_socket = fixture.directory.path().join("replacement.sock");
    fixture.sockets.insert(replacement_socket.clone());
    let replacement_raw = raw.replace("control.sock", "replacement.sock");
    fs::write(&fixture.config, &replacement_raw)?;
    let occupied = std::os::unix::net::UnixListener::bind(&replacement_socket)?;
    fixture
        .run(fixture.selected_command(full_binary, "restart"))?
        .rejected("replacement control endpoint is already in use")?;
    fixture.status(full_binary, &full, &gateway, gateway_pid)?;
    ensure!(fixture.hello(&gateway)? == gateway_instance);
    drop(occupied);
    fs::remove_file(&replacement_socket)?;
    fixture.socket = replacement_socket;

    let restart: serde_json::Value = fixture
        .run(fixture.selected_command(full_binary, "restart"))?
        .json()?;
    ensure!(
        restart["action"] == "restart" && restart["status"] == "started",
        "{restart}"
    );
    let replacement_pid = u32::try_from(restart["pid"].as_u64().context("restart omitted PID")?)?;
    ensure!(
        replacement_pid != gateway_pid,
        "replacement PID did not change: {restart}"
    );
    fixture.reap()?;
    ensure!(
        !daemon::process_is_alive(gateway_pid),
        "restart left old gateway alive"
    );
    ensure!(!old_socket.exists(), "restart retained old control socket");
    let report = fixture.status(full_binary, &full, &full, replacement_pid)?;
    fixture.health(&report)?;
    ensure!(
        fixture.hello(&full)? != gateway_instance,
        "replacement retained old instance identity"
    );
    fixture.status(gateway_binary, &gateway, &full, replacement_pid)?;
    ensure!(
        fs::read_to_string(&fixture.config)? == replacement_raw,
        "profile switch rewrote saved config"
    );
    ensure!(
        fixture.directory.path().join("profiles.db").is_file(),
        "private database missing"
    );
    Ok(())
}

#[test]
#[ignore = "requires separately built full and gateway artifacts"]
fn artifact_profiles_preserve_residents_and_gate_explicit_replacement() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let gateway = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    ensure!(
        full != gateway,
        "full and gateway must be separately preserved executables"
    );
    let mut fixture = Fixture::new()?;
    let result = exercise(&mut fixture, &full, &gateway);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "artifact profile acceptance failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup.with_context(|| format!("artifact profile cleanup failed\n{}", fixture.diagnostics()))
}
