use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent};
use futures::StreamExt;
use tokio::sync::mpsc;

/// All events the app loop consumes.
///
/// Phase 2 will add ACP variants (e.g. `AgentStatusChanged`, `MessageChunk`).
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal key press.
    Key(KeyEvent),
    /// Terminal resize (width, height). Redraw is handled by the main loop.
    Resize { _width: u16, _height: u16 },
    /// Tick / ignored terminal event.
    Tick,
}

/// Multiplexes terminal events (and future ACP events) into a single channel.
///
/// Holds the sender half so the channel stays alive. Phase 2 will expose
/// `sender()` for ACP workers to emit events.
pub struct EventHandler {
    _tx: mpsc::Sender<AppEvent>,
    rx: mpsc::Receiver<AppEvent>,
}

impl EventHandler {
    /// Create a new handler. Spawns a background task that pumps crossterm events.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        let pump_tx = tx.clone();
        tokio::spawn(terminal_event_pump(pump_tx));
        Self { _tx: tx, rx }
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
