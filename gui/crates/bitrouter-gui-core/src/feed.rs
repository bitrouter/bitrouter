use crate::protocol::*;
use futures::channel::mpsc;
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

/// Inbound event stream. A boxed [`Stream`] so any feed (mock, or the real
/// daemon feed later) can drive it on whatever executor polls it — no feed owns
/// a background thread, which keeps it deterministic under gpui's test scheduler.
pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

pub struct FeedHandle {
    pub events: EventStream,
    pub commands: mpsc::UnboundedSender<Command>,
}

/// A source of orchestrator events that also accepts commands. The real daemon
/// feed implements this same trait at upstream integration.
pub trait Feed {
    fn connect(self) -> FeedHandle;
}

/// Threadless mock event stream: replays a scripted burst, then synthesizes a
/// reply for each `SendPrompt` it receives. Driven entirely by the poller.
struct MockStream {
    queued: VecDeque<Event>,
    commands: mpsc::UnboundedReceiver<Command>,
}

impl Stream for MockStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(ev) = this.queued.pop_front() {
            return Poll::Ready(Some(ev));
        }
        loop {
            match Pin::new(&mut this.commands).poll_next(cx) {
                Poll::Ready(Some(Command::SendPrompt {
                    target: Target::Session { id },
                    text,
                })) => {
                    return Poll::Ready(Some(Event::SessionUpdate {
                        session: id,
                        update: SessionUpdateKind::Message {
                            text: format!("» {text}"),
                        },
                    }));
                }
                // Other commands have no scripted reply; keep draining.
                Poll::Ready(Some(_)) => continue,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct MockFeed {
    script: Vec<Event>,
}

impl MockFeed {
    pub fn new(script: Vec<Event>) -> Self {
        Self { script }
    }

    /// The overview scenario from the spec: three agents, one pending permission.
    pub fn scenario() -> Self {
        let mk = |id: &str, harness: &str, model: &str| Session {
            id: SessionId(id.into()),
            name: id.into(),
            tab: TabId("auth-feature".into()),
            harness: harness.into(),
            model: model.into(),
            status: SessionStatus::Running,
            render_mode: RenderMode::Terminal,
        };
        Self::new(vec![
            Event::AgentSpawned {
                session: mk("auth-fix", "claude-code", "claude-opus-4"),
            },
            Event::AgentSpawned {
                session: mk("refactor-api", "qwen-code", "qwen"),
            },
            Event::AgentSpawned {
                session: mk("add-tests", "gemini-cli", "gemini-flash"),
            },
            Event::RoutingDecided {
                session: SessionId("refactor-api".into()),
                route: Route {
                    asked: "claude".into(),
                    routed: "qwen".into(),
                    rule: "cost-gate".into(),
                },
            },
            Event::RequestCompleted {
                session: SessionId("auth-fix".into()),
                model: "claude-opus-4".into(),
                prompt_tokens: 8_000,
                completion_tokens: 2_000,
                cost_micro_usd: 420_000,
                latency_ms: 1_100,
                failed_over: false,
            },
            Event::RequestCompleted {
                session: SessionId("refactor-api".into()),
                model: "qwen".into(),
                prompt_tokens: 12_000,
                completion_tokens: 3_000,
                cost_micro_usd: 1_100_000,
                latency_ms: 900,
                failed_over: false,
            },
            Event::PermissionRequested {
                session: SessionId("refactor-api".into()),
                request_id: "p1".into(),
                summary: "WRITE src/api/mod.rs".into(),
                diff: Some("- pub mod handler;\n+ pub mod users; pub mod posts;".into()),
            },
        ])
    }
}

impl Feed for MockFeed {
    fn connect(self) -> FeedHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let stream = MockStream {
            queued: self.script.into_iter().collect(),
            commands: cmd_rx,
        };
        FeedHandle {
            events: Box::pin(stream),
            commands: cmd_tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::StreamExt;

    #[test]
    fn mock_emits_scenario_then_echoes_prompt() -> anyhow::Result<()> {
        let mut h = MockFeed::scenario().connect();
        let first = block_on(h.events.next()).ok_or_else(|| anyhow::anyhow!("no event"))?;
        assert!(matches!(first, Event::AgentSpawned { .. }));

        h.commands.unbounded_send(Command::SendPrompt {
            target: Target::Session {
                id: SessionId("auth-fix".into()),
            },
            text: "hi".into(),
        })?;

        // drain until we see the echoed SessionUpdate
        let echoed = block_on(async {
            while let Some(ev) = h.events.next().await {
                if matches!(ev, Event::SessionUpdate { .. }) {
                    return Some(ev);
                }
            }
            None
        })
        .ok_or_else(|| anyhow::anyhow!("no echo"))?;
        assert!(matches!(echoed, Event::SessionUpdate { .. }));
        Ok(())
    }
}
