use agent_client_protocol as acp;
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent};
use futures::StreamExt;
use tokio::sync::mpsc;

/// All events the app loop consumes.
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal key press.
    Key(KeyEvent),
    /// Terminal resize (width, height). Redraw is handled by the main loop.
    Resize { _width: u16, _height: u16 },
    /// Tick / ignored terminal event.
    Tick,
    /// Agent subprocess connected and ACP session created.
    AgentConnected { name: String },
    /// Agent-side error (spawn failure, protocol error, unexpected exit).
    AgentError { name: String, message: String },
    /// Streaming session update from the agent.
    SessionUpdate(acp::SessionNotification),
    /// Agent requests user permission for a tool call.
    PermissionRequest {
        request: acp::RequestPermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<acp::RequestPermissionResponse>,
    },
    /// The prompt turn completed (agent returned PromptResponse).
    PromptDone { _stop_reason: acp::StopReason },
}

/// Multiplexes terminal events and ACP events into a single channel.
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

    /// Clone the sender so ACP workers can emit events into the same channel.
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
            _ => AppEvent::Tick,
        };
        if tx.send(app_event).await.is_err() {
            break; // receiver dropped, app is shutting down
        }
    }
}
