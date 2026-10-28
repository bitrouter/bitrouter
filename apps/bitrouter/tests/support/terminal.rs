//! Canonical-terminal transport shared by onboarding and resource CLI tests.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;

pub struct Terminal {
    pub home: tempfile::TempDir,
    pub transcript: String,
    writer: Box<dyn Write + Send>,
    reader: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Terminal {
    pub fn spawn(home: tempfile::TempDir, command: CommandBuilder, size: PtySize) -> Result<Self> {
        let pair = native_pty_system().openpty(size)?;
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || tx.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            home,
            transcript: String::new(),
            writer,
            reader: rx,
            child,
            _master: pair.master,
        })
    }

    pub fn send(&mut self, input: &[u8]) -> Result<()> {
        self.writer.write_all(input)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn wait(&mut self, text: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.transcript.contains(text) {
            let bytes = self
                .reader
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .with_context(|| {
                    format!("waiting for {text:?}; transcript: {}", self.transcript)
                })?;
            self.transcript.push_str(&String::from_utf8_lossy(&bytes));
        }
        Ok(())
    }

    pub fn finish(&mut self, success: bool) -> Result<Value> {
        self.wait("__ONBOARDING_FIXTURE_DONE__")?;
        let result: Value =
            serde_json::from_slice(&std::fs::read(self.home.path().join("result.json"))?)?;
        ensure!(
            result["success"] == success && result["restored"] == true,
            "{result}: {}",
            self.transcript
        );
        ensure!(
            !self.transcript.contains("\u{1b}[?1049"),
            "alternate screen"
        );
        let stdout = std::fs::read(self.home.path().join("stdout"))?;
        ensure!(!stdout.contains(&0x1b), "terminal controls in JSON");
        Ok(serde_json::from_slice(&stdout)?)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
