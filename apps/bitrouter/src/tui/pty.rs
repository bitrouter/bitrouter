//! PTY hosting for the orchestrator pane (TUI_SPEC §2/§8a): spawn the native
//! harness as a PTY child, pump its output into the loop's channel, and own
//! the write side (input bytes + emulator responses).

use std::io::Write;

use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize};
use tokio::sync::mpsc::UnboundedSender;

use crate::tui::event::Incoming;
use crate::tui::term::{AlacrittyBackend, Osc52Scanner, TerminalBackend};

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
    child: Box<dyn portable_pty::Child + Send + Sync>,
    scanner: Osc52Scanner,
    size: (u16, u16),
}

impl PtyPane {
    /// Spawn the launch's `command args…` on a fresh PTY (cwd + env overlay
    /// applied) and start the reader thread that pumps output as
    /// [`Incoming::PtyOutput`], ending with [`Incoming::PtyExited`].
    pub fn spawn(
        record_id: &str,
        launch: &PtyLaunch<'_>,
        cols: u16,
        rows: u16,
        tx: UnboundedSender<Incoming>,
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
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawning '{command}' on the pty: {e}"))?;
        drop(pair.slave);

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
        let rid = record_id.to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx
                            .send(Incoming::PtyOutput {
                                record_id: rid.clone(),
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(Incoming::PtyExited { record_id: rid });
        });

        Ok(Self {
            backend: Box::new(AlacrittyBackend::new(cols, rows)),
            master: pair.master,
            writer,
            child,
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
        let _ = self.child.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Drive a real PTY child end to end: spawn, read output through the
    /// channel, feed the emulator, see the text in the grid.
    #[tokio::test]
    async fn pty_child_output_reaches_the_grid() {
        let (tx, mut rx) = unbounded_channel();
        let cwd = std::env::temp_dir();
        let args = [
            "-c".to_string(),
            "printf 'PTY_E2E_MARKER'; sleep 0.1".to_string(),
        ];
        let env = [("BITROUTER_TEST_VAR".to_string(), "1".to_string())];
        let mut pane = PtyPane::spawn(
            "orchestrator",
            &PtyLaunch {
                command: "sh",
                args: &args,
                env: &env,
                cwd: &cwd,
            },
            40,
            5,
            tx,
        )
        .expect("spawn pty child");

        let mut exited = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !exited && std::time::Instant::now() < deadline {
            let Ok(Some(incoming)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
            else {
                break;
            };
            match incoming {
                Incoming::PtyOutput { bytes, .. } => {
                    pane.feed(&bytes);
                }
                Incoming::PtyExited { .. } => exited = true,
                _ => {}
            }
        }
        assert!(exited, "child exit reaches the loop");
        let text: String = pane
            .backend
            .lines(true)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            text.contains("PTY_E2E_MARKER"),
            "child output rendered in the grid: {text:?}"
        );
        pane.kill();
    }

    /// Input written to the PTY reaches the child (cat echoes it back).
    #[tokio::test]
    async fn input_round_trips_through_the_child() {
        let (tx, mut rx) = unbounded_channel();
        let cwd = std::env::temp_dir();
        let mut pane = PtyPane::spawn(
            "orchestrator",
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
            let Ok(Some(incoming)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
            else {
                break;
            };
            if let Incoming::PtyOutput { bytes, .. } = incoming {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                pane.feed(&bytes);
            }
        }
        assert!(seen.contains("round-trip"), "echoed: {seen:?}");
        pane.kill();
    }
}
