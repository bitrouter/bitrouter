use std::path::PathBuf;

use bitrouter_providers::acp::types::AgentEvent;
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, MouseEvent};
use futures::StreamExt;
use tokio::sync::mpsc;

/// All events the app loop consumes.
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal key press.
    Key(KeyEvent),
    /// Terminal mouse event.
    Mouse(MouseEvent),
    /// Terminal resize.
    Resize { _width: u16, _height: u16 },
    /// Tick / ignored terminal event.
    Tick,
    /// An event from an ACP agent provider.
    Agent(AgentEvent),
    /// Binary agent install progress update.
    InstallProgress { agent_id: String, percent: u8 },
    /// Binary agent install completed.
    InstallComplete {
        agent_id: String,
        binary_path: PathBuf,
    },
    /// Binary agent install failed.
    InstallFailed { agent_id: String, message: String },
}

/// Multiplexes terminal events and agent events into a single channel.
pub struct EventHandler {
    tx: mpsc::Sender<AppEvent>,
    rx: mpsc::Receiver<AppEvent>,
}

impl EventHandler {
    /// Create a new handler. Spawns a background task that pumps crossterm events.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        let pump_tx = tx.clone();
        tokio::spawn(terminal_event_pump(pump_tx));
        Self { tx, rx }
    }

    /// Clone the sender so agent workers can emit events into the same channel.
    pub fn sender(&self) -> mpsc::Sender<AppEvent> {
        self.tx.clone()
    }

    /// Wait for the next event.
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }
}

async fn terminal_event_pump(tx: mpsc::Sender<AppEvent>) {
    let mut stream = EventStream::new();
    while let Some(Ok(event)) = stream.next().await {
        let app_event = match event {
            CrosstermEvent::Key(k) => AppEvent::Key(k),
            CrosstermEvent::Resize(w, h) => AppEvent::Resize {
                _width: w,
                _height: h,
            },
            CrosstermEvent::Mouse(m) => AppEvent::Mouse(m),
            _ => AppEvent::Tick,
        };
        if tx.send(app_event).await.is_err() {
            break; // receiver dropped, app is shutting down
        }
    }
}
