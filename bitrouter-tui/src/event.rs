use crossterm::event::{Event, EventStream};
use futures::StreamExt;

pub struct EventHandler {
    stream: EventStream,
}

impl EventHandler {
    pub fn new() -> Self {
        Self {
            stream: EventStream::new(),
        }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.stream.next().await?.ok()
    }
}
