//! PTY hosting for the orchestrator pane (TUI_SPEC §2/§8a): spawn the native
//! harness as a PTY child, pump its output into the loop's channel, and own
//! the write side (input bytes + emulator responses).

use std::io::Write;

use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize};
use tokio::sync::mpsc::UnboundedSender;

use super::term::{AlacrittyBackend, Osc52Scanner, TerminalBackend};

/// What the PTY reader thread reports to the host loop.
///
/// Replaces the orchestrator's `Incoming`, which carried pane-routing and ACP
/// variants this host has no use for. `PtyExited` gained the child's status:
/// `launch` promises the shell sees the agent's exit code, and the old enum
/// could not carry one.
#[derive(Debug)]
pub enum HostEvent {
    /// Bytes read from the child.
    Output(Vec<u8>),
    /// The child was reaped. Carries its exit code, when it had one.
    Exited(Option<i32>),
}

/// What to run on the PTY: command line, env overlay, working directory.
#[derive(Clone, Copy)]
pub struct PtyLaunch<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a std::path::Path,
}

/// A live PTY child + its emulator core. The reader thread pumps output into
/// the loop; the loop calls [`feed`](Self::feed) / [`write_input`](Self::write_input) /
/// [`resize`](Self::resize).
pub struct PtyPane {
    pub backend: Box<dyn TerminalBackend>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    scanner: Osc52Scanner,
    size: (u16, u16),
}

impl PtyPane {
    /// Spawn the launch's `command args…` on a fresh PTY (cwd + env overlay
    /// applied) and start the reader thread that pumps output as
    /// [`HostEvent::Output`], ending with [`HostEvent::Exited`].
    pub fn spawn(
        launch: &PtyLaunch<'_>,
        cols: u16,
        rows: u16,
        tx: UnboundedSender<HostEvent>,
    ) -> Result<Self> {
        let PtyLaunch {
            command,
            args,
            env,
            cwd,
        } = *launch;
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("opening pty: {e}"))?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        cmd.cwd(cwd);
        // The inner app sizes itself from the PTY; TERM must promise only what
        // the emulator renders (truecolor xterm, no graphics — capability
        // scoping for composited panes, §9).
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawning '{command}' on the pty: {e}"))?;
        drop(pair.slave);

        // Reaping runs on its own thread and owns the `Child`; the pane keeps
        // only a killer. Two reasons this is not folded into the reader
        // thread's EOF: a child can close its output and linger, so EOF is not
        // exit — and `wait` is the only place the exit code exists to be
        // captured at all, which `launch` promises to propagate.
        let killer = child.clone_killer();
        let exit_tx = tx.clone();
        std::thread::spawn(move || {
            let status = child.wait().ok().map(|status| status.exit_code() as i32);
            let _ = exit_tx.send(HostEvent::Exited(status));
        });

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("cloning pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("taking pty writer: {e}"))?;

        // Reader thread: blocking reads → loop channel. Ends (EOF/error) when
        // the child exits or the master is dropped at teardown.
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(HostEvent::Output(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            backend: Box::new(AlacrittyBackend::new(cols, rows)),
            master: pair.master,
            writer,
            killer,
            scanner: Osc52Scanner::default(),
            size: (cols, rows),
        })
    }

    /// Feed one output chunk into the emulator; returns any OSC-52 sequences
    /// to re-emit verbatim to the outer terminal, and flushes the emulator's
    /// own query responses back to the child.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let forwarded = self.scanner.scan(bytes);
        self.backend.feed(bytes);
        let responses = self.backend.drain_responses();
        if !responses.is_empty() {
            let _ = self.writer.write_all(&responses);
            let _ = self.writer.flush();
        }
        forwarded
    }

    /// Write already-encoded input bytes to the child.
    pub fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Resize the emulator and the PTY (delivers `SIGWINCH` to the child).
    /// No-op when the size is unchanged (debounces the per-frame check).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if self.size == (cols, rows) || cols < 2 || rows < 1 {
            return;
        }
        self.size = (cols, rows);
        self.backend.resize(cols, rows);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Kill the child (teardown).
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Read the grid as plain text.
    fn grid_text(pane: &PtyPane) -> String {
        pane.backend
            .lines(true)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// Drive a real PTY child end to end: spawn, read output through the
    /// channel, feed the emulator, see the text in the grid — and get the
    /// child's exit code back.
    #[tokio::test]
    async fn a_child_renders_into_the_grid_and_reports_its_exit_code() {
        let (tx, mut rx) = unbounded_channel();
        let cwd = std::env::temp_dir();
        let args = [
            "-c".to_string(),
            "printf 'PTY_E2E_MARKER'; exit 7".to_string(),
        ];
        let mut pane = PtyPane::spawn(
            &PtyLaunch {
                command: "sh",
                args: &args,
                env: &[],
                cwd: &cwd,
            },
            40,
            5,
            tx,
        )
        .expect("spawn pty child");

        let mut code = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while code.is_none() && std::time::Instant::now() < deadline {
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
            else {
                break;
            };
            match event {
                HostEvent::Output(bytes) => {
                    pane.feed(&bytes);
                }
                HostEvent::Exited(status) => code = Some(status),
            }
        }

        let text = grid_text(&pane);
        assert!(text.contains("PTY_E2E_MARKER"), "rendered: {text:?}");
        // The restored `pty.rs` could not do this: `PtyExited` carried no
        // status and the child was never reaped, so `launch --tui` had no exit
        // code to propagate.
        assert_eq!(
            code,
            Some(Some(7)),
            "the child's exit code reaches the host"
        );
        pane.kill();
    }

    /// Input written to the PTY reaches the child (cat echoes it back).
    #[tokio::test]
    async fn input_round_trips_through_the_child() {
        let (tx, mut rx) = unbounded_channel();
        let cwd = std::env::temp_dir();
        let mut pane = PtyPane::spawn(
            &PtyLaunch {
                command: "cat",
                args: &[],
                env: &[],
                cwd: &cwd,
            },
            40,
            5,
            tx,
        )
        .expect("spawn cat");
        pane.write_input(b"round-trip\r");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = String::new();
        while std::time::Instant::now() < deadline && !seen.contains("round-trip") {
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
            else {
                break;
            };
            if let HostEvent::Output(bytes) = event {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                pane.feed(&bytes);
            }
        }
        assert!(seen.contains("round-trip"), "echoed: {seen:?}");
        pane.kill();
    }

    /// A child that closes its output but keeps running must not be reported
    /// as exited — EOF is not exit, and treating it as one would tear down a
    /// live session.
    #[tokio::test]
    async fn closing_output_is_not_the_same_as_exiting() {
        let (tx, mut rx) = unbounded_channel();
        let cwd = std::env::temp_dir();
        let args = ["-c".to_string(), "exec 1>&-; sleep 0.6; exit 3".to_string()];
        let mut pane = PtyPane::spawn(
            &PtyLaunch {
                command: "sh",
                args: &args,
                env: &[],
                cwd: &cwd,
            },
            40,
            5,
            tx,
        )
        .expect("spawn pty child");

        let started = std::time::Instant::now();
        let mut code = None;
        while code.is_none() && started.elapsed() < std::time::Duration::from_secs(10) {
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
            else {
                break;
            };
            if let HostEvent::Exited(status) = event {
                code = Some(status);
            }
        }
        assert_eq!(code, Some(Some(3)), "the real exit status, not the EOF");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(400),
            "exit was reported at EOF instead of at wait()"
        );
        pane.kill();
    }
}
