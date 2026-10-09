//! Declared shell interpreter, streaming output and owned process cleanup.

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use bitrouter_ai::types::ToolResultOutput;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MAX_LIVE_CHUNKS, MAX_OUTPUT_BYTES, MAX_SHELL_SECONDS, ShellArgs, WorkspaceTools};
use crate::agent::RunEvent;
use crate::store::EffectStatus;

#[derive(Clone)]
pub(super) struct Interpreter {
    pub(super) executable: PathBuf,
    pub(super) dialect: &'static str,
}

impl Interpreter {
    pub(super) fn discover() -> std::io::Result<Self> {
        #[cfg(windows)]
        let candidates = [("pwsh.exe", "powershell"), ("powershell.exe", "powershell")];
        #[cfg(not(windows))]
        let candidates = [("bash", "bash"), ("sh", "sh")];
        Self::search(std::env::var_os("PATH").as_deref(), &candidates)
    }

    pub(super) fn search(
        search_path: Option<&std::ffi::OsStr>,
        candidates: &[(&str, &'static str)],
    ) -> std::io::Result<Self> {
        if let Some(search_path) = search_path {
            for (name, dialect) in candidates {
                for directory in std::env::split_paths(search_path) {
                    let path = directory.join(name);
                    let Ok(metadata) = fs::metadata(&path) else {
                        continue;
                    };
                    if !metadata.is_file() {
                        continue;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if metadata.permissions().mode() & 0o111 == 0 {
                            continue;
                        }
                    }
                    return Ok(Self {
                        executable: path.canonicalize()?,
                        dialect,
                    });
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no supported shell interpreter is available in the server PATH",
        ))
    }

    pub(super) fn identity(&self) -> serde_json::Value {
        serde_json::json!({"executable": self.executable, "dialect": self.dialect})
    }
}

impl WorkspaceTools {
    pub(super) async fn shell(
        &self,
        args: ShellArgs,
        cancel: &CancellationToken,
        tool_id: &str,
        live: Option<&mpsc::Sender<RunEvent>>,
        effect: &mut EffectStatus,
    ) -> Result<ToolResultOutput, String> {
        if args.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        let timeout = args.timeout.unwrap_or(30);
        if timeout == 0 || timeout > MAX_SHELL_SECONDS {
            return Err("timeout must be in 1..=120 seconds".into());
        }
        let interpreter = self
            .interpreter
            .as_ref()
            .ok_or("shell interpreter unavailable")?;
        let mut command = Command::new(&interpreter.executable);
        let command_text = if interpreter.dialect == "powershell" {
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
            ]);
            format!(
                "try {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}\n{}",
                args.command
            )
        } else {
            command.arg("-c");
            args.command
        };
        command
            .arg(&command_text)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        // The declared interpreter is fixed. Never retry a generated command
        // in another dialect after spawn failure or effects.
        let mut child = spawn_shell(command)
            .map_err(|error| format!("cannot launch declared shell: {error}"))?;
        // A launched command may already have changed external state.
        *effect = EffectStatus::Unknown;
        let child_id = child.id();
        #[cfg(unix)]
        let mut process_group = ProcessGroupGuard(child_id);
        #[cfg(not(windows))]
        let stdout = child.stdout.take().ok_or("stdout pipe unavailable")?;
        #[cfg(windows)]
        let stdout = child.stdout().take().ok_or("stdout pipe unavailable")?;
        #[cfg(not(windows))]
        let stderr = child.stderr.take().ok_or("stderr pipe unavailable")?;
        #[cfg(windows)]
        let stderr = child.stderr().take().ok_or("stderr pipe unavailable")?;
        let stdout_task = tokio::spawn(read_bounded(
            stdout,
            tool_id.to_owned(),
            "stdout",
            live.cloned(),
        ));
        let stderr_task = tokio::spawn(read_bounded(
            stderr,
            tool_id.to_owned(),
            "stderr",
            live.cloned(),
        ));
        let mut timed_out = false;
        let status = tokio::select! {
            result = wait_shell(&mut child) => result.map_err(|error| error.to_string())?,
            _ = cancel.cancelled() => {
                let cleanup = kill_shell(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                cleanup.map_err(|error| format!("command cleanup failed: {error}"))?;
                #[cfg(unix)]
                { process_group.0 = None; }
                return Err("command cancelled; effects may have occurred".into());
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                timed_out = true;
                kill_shell(&mut child).await.map_err(|error| error.to_string())?;
                wait_shell(&mut child).await.map_err(|error| error.to_string())?
            }
        };
        // A shell may exit while a background child still holds a pipe open.
        stop_shell_descendants(&mut child, child_id)
            .await
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            process_group.0 = None;
        }
        let (stdout, stdout_truncated) = stdout_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let (stderr, stderr_truncated) = stderr_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        if !timed_out {
            *effect = EffectStatus::Completed;
        }
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({
                "interpreter": interpreter.identity(),
                "exit_status": status.code(), "stdout": stdout, "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated, "timed_out": timed_out
            }),
        })
    }
}

#[cfg(windows)]
type ShellChild = Box<dyn process_wrap::tokio::ChildWrapper>;
#[cfg(not(windows))]
type ShellChild = tokio::process::Child;

#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

fn spawn_shell(command: Command) -> std::io::Result<ShellChild> {
    #[cfg(windows)]
    {
        use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
        // Suspend during assignment so even immediate descendants belong to
        // the job; KillOnDrop also covers errors and dropped execution futures.
        CommandWrap::from(command)
            .wrap(KillOnDrop)
            .wrap(JobObject)
            .spawn()
    }
    #[cfg(not(windows))]
    {
        let mut command = command;
        command.spawn()
    }
}

async fn wait_shell(child: &mut ShellChild) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        // Wait only for the shell. JobObject's wait includes descendants,
        // which must be terminated before draining inherited output pipes.
        child.inner_mut().wait().await
    }
    #[cfg(not(windows))]
    child.wait().await
}

async fn kill_shell(child: &mut ShellChild) -> std::io::Result<()> {
    let child_id = child.id();
    stop_shell_descendants(child, child_id).await?;
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(not(windows))]
    child.kill().await
}

async fn stop_shell_descendants(
    _child: &mut ShellChild,
    _child_id: Option<u32>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        _child.start_kill()?;
        // The task is terminal only after every job process has exited.
        _child.wait().await?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        kill_process_group(_child_id);
        Ok(())
    }
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid
        && let Ok(raw) = i32::try_from(pid)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(any(unix, windows)))]
fn kill_process_group(_pid: Option<u32>) {}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    tool_id: String,
    source: &'static str,
    live: Option<mpsc::Sender<RunEvent>>,
) -> std::io::Result<(String, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut live_chunks = 0;
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        let take = read.min(remaining);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < read;
        if take > 0
            && live_chunks < MAX_LIVE_CHUNKS
            && let Some(sender) = &live
        {
            let _ = sender
                .send(RunEvent::ToolOutputDelta {
                    id: tool_id.clone(),
                    source: source.into(),
                    text: String::from_utf8_lossy(&chunk[..take]).into_owned(),
                })
                .await;
            live_chunks += 1;
        }
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}
